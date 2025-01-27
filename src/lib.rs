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
    _share: PhantomData<*const T>
}

unsafe impl<T: Send> Send for ShareBox<T> {}

unsafe impl<T: Sync> Sync for ShareBox<T> {}

impl<T> ShareBox<T> {
    pub fn new(value: T) -> Self {
        ShareBox {
            ptr: SharedPtr::new(value),
             _share: PhantomData
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
        let borrow = shared.inner().data.get() as *mut T;
        let ptr = shared.clone();
        (
            RefShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                borrow,
            },
            Ref {
                ptr: BorrowPtr { shared, index },
                borrow,
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
        let borrow = shared.inner().data.get() as *mut T;
        let ptr = shared.clone();
        (
            RefMutShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                borrow,
            },
            RefMut {
                ptr: BorrowPtr { shared, index },
                borrow,
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
        let borrow = shared.inner().data.get() as *const T;
        Ref {
            ptr: BorrowPtr { shared, index },
            borrow,
        }

    }

    pub fn into_mut(self) -> RefMut<T> {
        let index = graph::END;
        let shared = self.into_shared();
        let borrow = shared.inner().data.get() as *mut T;
        RefMut {
            ptr: BorrowPtr { shared, index },
            borrow,
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
pub struct Weak<T, B: ?Sized = T> {
    shared: SharedPtr<T>,
    borrow: *const B,
    index: usize,
    version: u64,
}

unsafe impl<T: Send, B: ?Sized + Sync> Send for Weak<T, B> {}

unsafe impl<T: Send, B: ?Sized + Sync> Sync for Weak<T, B> {}

impl<T, B: ?Sized> Weak<T, B> {
    pub fn upgrade(self) -> Option<Ref<T, B>> {
        let mut graph = self.shared.inner().graph.lock().unwrap();
        unsafe { graph.try_upgrade_weak_unchecked(self.index, self.version) }
            .then_some(drop(graph))
            .map(|_| Ref {
                ptr: BorrowPtr { shared: self.shared, index: self.index, },
                borrow: self.borrow,
            })
    }
}

pub struct Ref<T, B: ?Sized = T> {
    ptr: BorrowPtr<T>,
    borrow: *const B,
}

unsafe impl<T: Send, B: ?Sized + Sync> Send for Ref<T, B> {}

unsafe impl<T: Send, B: ?Sized + Sync> Sync for Ref<T, B> {}

impl<T, B: ?Sized> Ref<T, B> {
    pub fn downgrade(&self) -> Weak<T, B> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let version = unsafe { graph.track_weak_unchecked(self.ptr.index) };
        Weak {
            shared: self.ptr.shared.clone(),
            borrow: self.borrow,
            index: self.ptr.index,
            version,
        }
    }

    /// Given that `f` works for all lifetimes `'a`, it will also work for
    /// the indeterminable, yet existant, lifetime of the `BorrowInner`.
    pub fn map<C: ?Sized>(self, f: impl for<'a> FnOnce(&'a B) -> &'a C) -> Ref<T, C> {
        Ref {
            ptr: self.ptr,
            borrow: f(unsafe { self.borrow.as_ref().unwrap_unchecked() }) as *const C
        }
    }

    pub fn into_deref(self) -> Ref<T, B::Target> where B: Deref {
        Ref::map(self, B::deref)
    }
}

impl<T, B: ?Sized> Deref for Ref<T, B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        unsafe { self.borrow.as_ref().unwrap_unchecked() }
    }
}

impl<T, B: ?Sized> Clone for Ref<T, B> {
    fn clone(&self) -> Self {
        Ref {
            ptr: self.ptr.clone(),
            borrow: self.borrow
        }
    }
}

pub struct RefMut<T, B: ?Sized = T> {
    ptr: BorrowPtr<T>,
    borrow: *mut B,
}

unsafe impl<T: Send, B: ?Sized + Send> Send for RefMut<T, B> {}

unsafe impl<T, B: ?Sized + Sync> Sync for RefMut<T, B> {}

impl<T, B: ?Sized> RefMut<T, B> {
    pub fn into_ref(self) -> Ref<T, B> {
        let RefMut {
            ptr,
            borrow
        } = self;
        Ref {
            ptr,
            borrow,
        }
    }

    pub fn forward_ref(self) -> (RefForward<T, B>, Ref<T, B>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        drop(graph);
        let shared = SharedPtr { inner: self.ptr.inner };
        let borrow = self.borrow;
        let ptr = shared.clone();
        std::mem::forget(self.ptr);
        (
            RefForward {
                ptr: FuturePtr { shared: Some(ptr), index },
                borrow,
            },
            Ref {
                ptr: BorrowPtr { shared, index },
                borrow,
            }
        )
    }

    pub fn forward_mut(self) -> (RefMutForward<T, B>, RefMut<T, B>) {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        drop(graph);
        let shared = SharedPtr { inner: self.ptr.inner };
        let borrow = self.borrow;
        let ptr = shared.clone();
        std::mem::forget(self.ptr);
        (
            RefMutForward {
                ptr: FuturePtr { shared: Some(ptr), index },
                borrow,
            },
            RefMut {
                ptr: BorrowPtr { shared, index },
                borrow,
            }
        )
    }

    pub fn cleave_ref<U>(self, f: impl FnOnce(Ref<T, B>) -> U) -> RefForward<T, B> {
        let (fut, ptr) = self.forward_ref();
        f(ptr);
        fut
    }

    pub fn cleave_mut<U>(self, f: impl FnOnce(RefMut<T, B>) -> U) -> RefMutForward<T, B> {
        let (fut, ptr) = self.forward_mut();
        f(ptr);
        fut
    }

    /// Given that `f` works for all lifetimes `'a`, it will also work for
    /// the indeterminable, yet existant, lifetime of the `BorrowInner`.
    pub fn map<C: ?Sized>(self, f: impl for<'a> FnOnce(&'a mut B) -> &'a mut C) -> RefMut<T, C> {
        RefMut {
            ptr: self.ptr,
            borrow: f(unsafe { self.borrow.as_mut().unwrap_unchecked() }) as *mut C
        }
    }

    pub fn into_deref(self) -> RefMut<T, B::Target> where B: DerefMut {
        RefMut::map(self, B::deref_mut)
    }
}

impl<T, B: ?Sized> Deref for RefMut<T, B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        unsafe { self.borrow.as_ref().unwrap_unchecked() }
    }
}

impl<T, B: ?Sized> DerefMut for RefMut<T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.borrow.as_mut().unwrap_unchecked() }
    }
}

// MARK: Futures

pub struct RefShare<T> {
    ptr: FuturePtr<T, First>,
    borrow: *const T,
}

unsafe impl<T: Send + Sync> Send for RefShare<T> {}

unsafe impl<T: Send + Sync> Sync for RefShare<T> {}

impl<T> RefShare<T> {
    pub fn get(&self) -> &T {
        unsafe { self.ptr.shared.as_ref().unwrap().get_ref() }
    }

    pub fn as_ref(&self) -> Ref<T> {
        let shared = self.ptr.shared.as_ref().unwrap().clone();
        let borrow = shared.inner().data.get() as *mut T;
        let mut graph = shared.inner().graph.lock().unwrap();
        let index = self.ptr.index.get();
        unsafe { graph.track_borrow_unchecked(index) };
        drop(graph);
        Ref {
            ptr: BorrowPtr { shared, index },
            borrow,
        }
    }

    pub fn generalise(self) -> RefForward<T> {
        let borrow = self.ptr.shared.as_ref().unwrap().inner().data.get() as *mut T;
        RefForward { ptr: self.ptr.generalise(), borrow }
    }
}

impl<T> Deref for RefShare<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.borrow.as_ref().unwrap_unchecked() }
    }
}

impl<T> Future for RefShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_shared(cx).map(|ptr| ShareBox { ptr, _share: PhantomData })
    }
}

impl<T> FusedFuture for RefShare<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

pub struct RefMutShare<T> {
    ptr: FuturePtr<T, First>,
    borrow: *const T,
}

unsafe impl<T: Send> Send for RefMutShare<T> {}

unsafe impl<T: Sync> Sync for RefMutShare<T> {}

impl<T> RefMutShare<T> {
    pub fn generalise(self) -> RefMutForward<T> {
        RefMutForward { ptr: self.ptr.generalise(), borrow: self.borrow as *mut T }
    }
}

impl<T> Future for RefMutShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_shared(cx).map(|ptr| ShareBox { ptr, _share: PhantomData })
    }
}

impl<T> FusedFuture for RefMutShare<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

impl<T> From<RefShare<T>> for RefMutShare<T> {
    fn from(value: RefShare<T>) -> Self {
        RefMutShare { ptr: value.ptr, borrow: value.borrow }
    }
}

pub struct RefForward<T, B: ?Sized = T> {
    ptr: FuturePtr<T, usize>,
    borrow: *mut B,
}

unsafe impl<T: Send, B: ?Sized + Sync> Send for RefForward<T, B> {}

unsafe impl<T: Send, B: ?Sized + Sync> Sync for RefForward<T, B> {}

impl<T, B: ?Sized> RefForward<T, B> {
    pub fn get(&self) -> &T {
        unsafe { self.ptr.shared.as_ref().unwrap().get_ref() }
    }

    pub fn as_ref(&self) -> Ref<T, B> {
        let shared = self.ptr.shared.as_ref().unwrap().clone();
        let borrow = self.borrow;
        let mut graph = shared.inner().graph.lock().unwrap();
        let index = self.ptr.index.get();
        unsafe { graph.track_borrow_unchecked(index) };
        drop(graph);
        Ref {
            ptr: BorrowPtr { shared, index },
            borrow
        }
    }
}

impl<T, B: ?Sized> Deref for RefForward<T, B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        unsafe { self.borrow.as_ref().unwrap_unchecked() }
    }
}

impl<T, B: ?Sized> Future for RefForward<T, B> {
    type Output = RefMut<T, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_borrow(cx).map(|ptr| RefMut { ptr, borrow: self.borrow })
    }
}

impl<T, B: ?Sized> FusedFuture for RefForward<T, B> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

pub struct RefMutForward<T, B: ?Sized = T> {
    ptr: FuturePtr<T, usize>,
    borrow: *mut B,
}

unsafe impl<T: Send, B: ?Sized + Send> Send for RefMutForward<T, B> {}

unsafe impl<T, B: ?Sized + Sync> Sync for RefMutForward<T, B> {}

impl<T, B: ?Sized> Future for RefMutForward<T, B> {
    type Output = RefMut<T, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_borrow(cx).map(|ptr| RefMut { ptr, borrow: self.borrow })
    }
}

impl<T, B: ?Sized> FusedFuture for RefMutForward<T, B> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

impl<T, B: ?Sized> From<RefForward<T, B>> for RefMutForward<T, B> {
    fn from(value: RefForward<T, B>) -> Self {
        RefMutForward { ptr: value.ptr, borrow: value.borrow }
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