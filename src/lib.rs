use std::{cell::UnsafeCell, future::Future, marker::PhantomData, mem::MaybeUninit, ops::{Deref, DerefMut}, pin::Pin, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex}, usize};

use futures::future::FusedFuture;

mod graph;
pub mod scope;

use graph::Graph;

// MARK: Inner

#[repr(align(4))] // aligned manually for 2 bits of extra space.
struct BorrowInner<T> {
    weak: AtomicUsize,
    graph: Mutex<Graph>,
    data: UnsafeCell<MaybeUninit<T>>
}

// MARK: Raw Pointers

struct SharedPtr<T> {
    inner: NonNull<BorrowInner<T>>
}

unsafe impl<T> Send for SharedPtr<T> {}

unsafe impl<T> Sync for SharedPtr<T> {}

impl<T> SharedPtr<T> {
    pub fn new(data: T) -> Self {
        SharedPtr {
            inner: unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(BorrowInner {
                weak: 1.into(),
                graph: Mutex::new(Graph::new()),
                data: UnsafeCell::new(MaybeUninit::new(data)),
            }))) }
        }
    }

    pub fn inner(&self) -> &BorrowInner<T> {
        unsafe { self.inner.as_ref() }
    }

    pub unsafe fn get_ref(&self) -> &T {
        self.inner().data.get().as_ref().unwrap_unchecked().assume_init_ref()
    }

    pub unsafe fn get_mut(&mut self) -> &mut T {
        self.inner().data.get().as_mut().unwrap_unchecked().assume_init_mut()
    }
}

impl<T> Clone for SharedPtr<T> {
    fn clone(&self) -> Self {
        self.inner().weak.fetch_add(1, Ordering::Relaxed);
        SharedPtr { inner: self.inner }
    }
}

impl<T> Drop for SharedPtr<T> {
    fn drop(&mut self) {
        if self.inner().weak.fetch_sub(1, Ordering::Release) == 1 {
            drop(unsafe { Box::from_raw(self.inner.as_ptr()) })
        }
    }
}

struct BorrowPtr<T> {
    shared: SharedPtr<T>,
    index: usize,
}

impl<T> Deref for BorrowPtr<T> {
    type Target = SharedPtr<T>;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl<T> DerefMut for BorrowPtr<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared
    }
}

impl<T> Clone for BorrowPtr<T> {
    fn clone(&self) -> Self {
        let mut graph = self.inner().graph.lock().unwrap();
        unsafe { graph.track_borrow_unchecked(self.index) };
        BorrowPtr {
            shared: self.shared.clone(),
            index: self.index
        }
    }
}

impl<T> Drop for BorrowPtr<T> {
    fn drop(&mut self) {
        let Ok(mut graph) = self.inner().graph.lock() else { return }; // TODO: handle poisoning
        unsafe {
            if graph.untrack_borrow_unchecked(self.index) {
                self.inner().data.get().as_mut().unwrap_unchecked().assume_init_drop();
            }
        }
    }
}

trait Index: Unpin {
    fn get(&self) -> usize;
}

impl Index for usize {
    fn get(&self) -> usize {
        *self
    }
}

struct First;

impl Index for First {
    fn get(&self) -> usize {
        0
    }
}

struct FuturePtr<T, I: Index> {
    shared: Option<SharedPtr<T>>,
    index: I
}

impl<T, I: Index> FuturePtr<T, I> {
    pub fn generalise(mut self) -> FuturePtr<T, usize> {
        FuturePtr { shared: self.shared.take(), index: self.index.get() }
    }

    pub fn poll_shared(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<SharedPtr<T>> {
        Pin::new(self).poll(cx).map(|(_, shared)| shared)
    }

    pub fn poll_borrow(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<BorrowPtr<T>> {
        Pin::new(self).poll(cx).map(|(index, shared)|
            BorrowPtr { shared, index }
        )
    }
} 

impl<T, I: Index> Future for FuturePtr<T, I> {
    type Output = (usize, SharedPtr<T>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let mut graph = self.shared.as_ref().unwrap().inner().graph.lock().unwrap();
        let poll = unsafe { graph.poll_unchecked(self.index.get(), cx) };
        drop(graph); // release mutex
        poll.map(|index| (index, unsafe { self.shared.take().unwrap_unchecked() }))
    }
}

impl<T, I: Index> FusedFuture for FuturePtr<T, I> {
    fn is_terminated(&self) -> bool {
        self.shared.is_none()
    }
}

impl<T, I: Index> Drop for FuturePtr<T, I> {
    fn drop(&mut self) {
        let Some(shared) = self.shared.as_ref() else { return };
        let mut graph = shared.inner().graph.lock().unwrap();
        if unsafe { !graph.close_future_unchecked(self.index.get()) } {
            drop(graph); // release mutex
            unsafe { drop(self.shared.take().unwrap_unchecked()) }
        }
    }
}

// MARK: Smart Pointers

pub struct ShareBox<T> {
    ptr: SharedPtr<T>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for ShareBox<T> {}

unsafe impl<T: Sync> Sync for ShareBox<T> {}

impl<T> ShareBox<T> {
    pub fn new(val: T) -> Self {
        ShareBox {
            ptr: SharedPtr::new(val),
             _variance: PhantomData
        }
    }

    fn into_shared(self) -> SharedPtr<T> {
        let ptr = SharedPtr { inner: self.ptr.inner };
        std::mem::forget(self);
        ptr
    }

    pub fn share_ref(self) -> (RefShare<T>, Ref<T>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        debug_assert_eq!(graph.share(), 0, r#"
            Graph should only have one root and it must be the last to be freed,
            hence it should be at the start of the free list / stack
            "#
        );
        let index = 0;
        drop(graph);
        let shared = self.into_shared();
        let ptr = shared.clone();
        (
            RefShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                _variance: PhantomData,
            },
            Ref {
                ptr: BorrowPtr { shared, index },
                _variance: PhantomData,
            }
        )
    }

    pub fn share_mut(self) -> (RefMutShare<T>, RefMut<T>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        debug_assert_eq!(graph.share(), 0, r#"
            Graph should only have one root and it must be the last to be freed,
            hence it should be at the start of the free list / stack
            "#
        );
        let index = 0;
        drop(graph);
        let shared = self.into_shared();
        let ptr = shared.clone();
        (
            RefMutShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                _variance: PhantomData,
            },
            RefMut {
                ptr: BorrowPtr { shared, index },
                _variance: PhantomData,
            }
        )
    }

    pub fn spawn_ref<U>(self, f: impl FnOnce(Ref<T>) -> U) -> RefShare<T> {
        let (fut, ptr) = self.share_ref();
        f(ptr);
        fut
    }

    pub fn spawn_mut<U>(self, f: impl FnOnce(RefMut<T>) -> U) -> RefMutShare<T> {
        let (fut, ptr) = self.share_mut();
        f(ptr);
        fut
    }

    pub fn into_ref(self) -> Ref<T> {
        let index = graph::END;
        let shared = self.into_shared();
        Ref {
            ptr: BorrowPtr { shared, index },
            _variance: PhantomData
        }

    }

    pub fn into_mut(self) -> RefMut<T> {
        let index = graph::END;
        let shared = self.into_shared();
        RefMut {
            ptr: BorrowPtr { shared, index },
            _variance: PhantomData
        }
    }

    pub fn into_inner(self) -> T {
        unsafe { self.into_shared().inner().data.get().as_mut().unwrap_unchecked().assume_init_read() }
    }
}

impl<T> Deref for ShareBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.get_ref() }
    }
}

impl<T> DerefMut for ShareBox<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.ptr.get_mut() }
    }
}

impl<T: Clone> Clone for ShareBox<T> {
    fn clone(&self) -> Self {
        ShareBox::new(T::clone(&*self))
    }
}

impl<T> Drop for ShareBox<T> {
    fn drop(&mut self) {
        unsafe { self.ptr.inner().data.get().as_mut().unwrap_unchecked().assume_init_drop() }
    }
}

#[derive(Clone)]
pub struct Weak<T> {
    shared: SharedPtr<T>,
    _variance: PhantomData<*const T>,
    index: usize,
    version: u64,
}

unsafe impl<T: Send> Send for Weak<T> {}

unsafe impl<T: Send> Sync for Weak<T> {}

impl<T> Weak<T> {
    pub fn upgrade(self) -> Option<Ref<T>> {
        let mut graph = self.shared.inner().graph.lock().unwrap();
        unsafe { graph.try_upgrade_weak_unchecked(self.index, self.version) }
            .then_some(drop(graph))
            .map(|_| Ref {
                ptr: BorrowPtr { shared: self.shared, index: self.index },
                _variance: PhantomData
            })
    }
}

pub struct Ref<T> {
    ptr: BorrowPtr<T>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for Ref<T> {}

unsafe impl<T: Send> Sync for Ref<T> {}

impl<T> Ref<T> {
    pub fn downgrade(&self) -> Weak<T> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let version = unsafe { graph.track_weak_unchecked(self.ptr.index) };
        Weak {
            shared: self.ptr.shared.clone(),
            _variance: PhantomData,
            index: self.ptr.index,
            version,
        }
    }
}

impl<T> Deref for Ref<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.get_ref() }
    }
}

impl<T> Clone for Ref<T> {
    fn clone(&self) -> Self {
        Ref {
            ptr: self.ptr.clone(),
            _variance: PhantomData
        }
    }
}

pub struct RefMut<T> {
    ptr: BorrowPtr<T>,
    _variance: PhantomData<*mut T>
}

unsafe impl<T: Send> Send for RefMut<T> {}

unsafe impl<T: Sync> Sync for RefMut<T> {}

impl<T> RefMut<T> {
    pub fn into_ref(self) -> Ref<T> {
        let RefMut {
            ptr,
            _variance
        } = self;
        Ref {
            ptr,
            _variance: PhantomData,
        }
    }

    pub fn forward_ref(self) -> (RefForward<T>, Ref<T>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        drop(graph);
        let shared = SharedPtr { inner: self.ptr.inner };
        let ptr = shared.clone();
        std::mem::forget(self.ptr);
        (
            RefForward {
                ptr: FuturePtr { shared: Some(ptr), index },
                _variance: PhantomData,
            },
            Ref {
                ptr: BorrowPtr { shared, index },
                _variance: PhantomData,
            }
        )
    }

    pub fn forward_mut(self) -> (RefMutForward<T>, RefMut<T>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        drop(graph);
        let shared = SharedPtr { inner: self.ptr.inner };
        let ptr = shared.clone();
        std::mem::forget(self.ptr);
        (
            RefMutForward {
                ptr: FuturePtr { shared: Some(ptr), index },
                _variance: PhantomData,
            },
            RefMut {
                ptr: BorrowPtr { shared, index },
                _variance: PhantomData,
            }
        )
    }

    pub fn cleave_ref<U>(self, f: impl FnOnce(Ref<T>) -> U) -> RefForward<T> {
        let (fut, ptr) = self.forward_ref();
        f(ptr);
        fut
    }

    pub fn cleave_mut<U>(self, f: impl FnOnce(RefMut<T>) -> U) -> RefMutForward<T> {
        let (fut, ptr) = self.forward_mut();
        f(ptr);
        fut
    }
}

impl<T> Deref for RefMut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.get_ref() }
    }
}

impl<T> DerefMut for RefMut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.ptr.get_mut() }
    }
}

// MARK: Futures

pub struct RefShare<T> {
    ptr: FuturePtr<T, First>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for RefShare<T> {}

unsafe impl<T: Sync> Sync for RefShare<T> {}

impl<T> RefShare<T> {
    pub fn get(&self) -> &T {
        unsafe { self.ptr.shared.as_ref().unwrap().get_ref() }
    }

    pub fn as_ref(&self) -> Ref<T> {
        let shared = self.ptr.shared.as_ref().unwrap().clone();
        let mut graph = shared.inner().graph.lock().unwrap();
        let index = self.ptr.index.get();
        unsafe { graph.track_borrow_unchecked(index) };
        drop(graph);
        Ref {
            ptr: BorrowPtr { shared, index },
            _variance: PhantomData,
        }
    }

    pub fn generalise(self) -> RefForward<T> {
        RefForward { ptr: self.ptr.generalise(), _variance: PhantomData }
    }
}

impl<T> Future for RefShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_shared(cx).map(|ptr| ShareBox { ptr, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefShare<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

pub struct RefMutShare<T> {
    ptr: FuturePtr<T, First>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for RefMutShare<T> {}

unsafe impl<T: Sync> Sync for RefMutShare<T> {}

impl<T> RefMutShare<T> {
    pub fn generalise(self) -> RefMutForward<T> {
        RefMutForward { ptr: self.ptr.generalise(), _variance: PhantomData }
    }
}

impl<T> Future for RefMutShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_shared(cx).map(|ptr| ShareBox { ptr, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefMutShare<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

impl<T> From<RefShare<T>> for RefMutShare<T> {
    fn from(value: RefShare<T>) -> Self {
        RefMutShare { ptr: value.ptr, _variance: PhantomData }
    }
}

pub struct RefForward<T> {
    ptr: FuturePtr<T, usize>,
    _variance: PhantomData<*mut T>
}

unsafe impl<T: Send> Send for RefForward<T> {}

unsafe impl<T: Sync> Sync for RefForward<T> {}

impl<T> RefForward<T> {
    pub fn get(&self) -> &T {
        unsafe { self.ptr.shared.as_ref().unwrap().get_ref() }
    }

    pub fn as_ref(&mut self) -> Ref<T> {
        let shared = self.ptr.shared.as_ref().unwrap().clone();
        let mut graph = shared.inner().graph.lock().unwrap();
        let index = self.ptr.index.get();
        unsafe { graph.track_borrow_unchecked(index) };
        drop(graph);
        Ref {
            ptr: BorrowPtr { shared, index },
            _variance: PhantomData,
        }
    }
}

impl<T> Future for RefForward<T> {
    type Output = RefMut<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_borrow(cx).map(|ptr| RefMut { ptr, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefForward<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

pub struct RefMutForward<T> {
    ptr: FuturePtr<T, usize>,
    _variance: PhantomData<*mut T>
}

unsafe impl<T: Send> Send for RefMutForward<T> {}

unsafe impl<T: Sync> Sync for RefMutForward<T> {}

impl<T> Future for RefMutForward<T> {
    type Output = RefMut<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_borrow(cx).map(|ptr| RefMut { ptr, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefMutForward<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

impl<T> From<RefForward<T>> for RefMutForward<T> {
    fn from(value: RefForward<T>) -> Self {
        RefMutForward { ptr: value.ptr, _variance: PhantomData }
    }
}

// MARK: Tests

#[cfg(test)]
mod test {
    use std::sync::Mutex;

    use crate::ShareBox;

    #[tokio::test]
    async fn vibe_check() {
        use tokio::{task::spawn, time::{sleep, Duration}};
        let mut example = ShareBox::new(Mutex::new(0));
        *example.get_mut().unwrap() += 1;
        let mut example = example
            // borrow mut
            .spawn_mut(|mut example| spawn(async move {
                *example.get_mut().unwrap() += 1;
            }))
            .await
            // borrow mut
            .spawn_mut(|example| spawn(async move {
                *example
                    // forwarding borrow mut
                    .cleave_mut(|mut example| spawn(async move {
                        *example.get_mut().unwrap() += 1;
                    }))
                    .await
                    // forwarding borrow
                    .cleave_ref(|example| spawn(async move {
                        let a = example.clone();
                        let b = example.clone();
                        let c = example;
                        spawn(async move {
                            sleep(Duration::from_millis(3)).await;
                            *a.lock().unwrap() += 1;
                        });
                        spawn(async move {
                            sleep(Duration::from_millis(2)).await;
                            *b.lock().unwrap() += 1;
                        });
                        spawn(async move {
                            sleep(Duration::from_millis(1)).await;
                            *c.lock().unwrap() += 1;
                        });
                    }))
                    .await
                    // reaquisition
                    .get_mut().unwrap() += 1;
            }))
            .await
            // borrow mut
            .spawn_ref(|example| spawn(async move {
                let a = example.clone();
                let b = example.clone();
                let c = example;
                spawn(async move {
                    sleep(Duration::from_millis(3)).await;
                    *a.lock().unwrap() += 1;
                });
                spawn(async move {
                    sleep(Duration::from_millis(2)).await;
                    *b.lock().unwrap() += 1;
                });
                spawn(async move {
                    sleep(Duration::from_millis(1)).await;
                    *c.lock().unwrap() += 1;
                });
            }))
            .await
            // borrow
            .spawn_mut(|mut example| spawn(async move {
                *example.get_mut().unwrap() += 1;
            }))
            .await;
        // reaquisition
        *example.get_mut().unwrap() += 1;
        assert_eq!(12, example.into_inner().into_inner().unwrap());
    }

    #[test]
    fn new_drop() {
        let example = ShareBox::new(());
        drop(example);
    }

    #[test]
    fn new_take() {
        let example = ShareBox::new(());
        example.into_inner();
    }

    #[test]
    fn new_into_ref() {
        let example = ShareBox::new(());
        drop(example.into_ref());
    }

    #[tokio::test]
    async fn lil_test() {
        let example = ShareBox::new(());
        let (fut, rm) = example.share_mut();
        drop(rm);
        let example = fut.await;
        example.into_inner();
    }
}