use std::{cell::UnsafeCell, fmt::Debug, future::Future, marker::PhantomData, mem::MaybeUninit, ops::{Deref, DerefMut}, pin::Pin, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex}, task::Waker, usize};

use futures::{future::FusedFuture, FutureExt};

pub(crate) mod contract;
pub mod scope;
pub mod collections;

// MARK: Inner

#[derive(Debug)]
struct BorrowInner<T> {
    weak: AtomicUsize,
    state: Mutex<BorrowState>,
    data: UnsafeCell<MaybeUninit<T>>
}

#[derive(Debug)]
struct BorrowState {
    strong: usize,
    mut_toggles: u64,
    stack: Vec<BorrowWaker>,
}

impl BorrowState {
    pub fn is_mut(&self) -> bool {
        self.mut_toggles & 1 == 0
    }

    pub fn shake(&mut self) {
        if !self.is_mut() {
            self.drain_immutable();
        }
    }

    pub fn drain_immutable(&mut self) {
        debug_assert_eq!(self.is_mut(), false);
        while let Some(next) = self.stack.last() {
            match next {
                BorrowWaker::Skip => {
                    self.stack.pop();
                },
                BorrowWaker::Ref(_) => {
                    self.strong += 1;
                    unsafe { self.stack.pop().unwrap_unchecked() }.wake();
                },
                BorrowWaker::Mut(_) => return,
            }
        }
    }

    pub fn drain_empty(&mut self) {
        debug_assert_eq!(self.is_mut(), false);
        while let Some(next) = self.stack.last() {
            match next {
                BorrowWaker::Skip => {
                    self.stack.pop();
                },
                BorrowWaker::Ref(_) => {
                    self.strong += 1;
                    unsafe { self.stack.pop().unwrap_unchecked() }.wake();
                    self.drain_immutable();
                },
                BorrowWaker::Mut(_) => {
                    self.strong += 1;
                    self.mut_toggles += 1; // now mutable
                    unsafe { self.stack.pop().unwrap_unchecked() }.wake();
                    return
                },
            }
        }

    }
}

#[derive(Debug)]
enum BorrowWaker {
    Skip,
    Ref(Waker),
    Mut(Waker),
}

impl BorrowWaker {
    pub fn wake(self) {
        match self {
            BorrowWaker::Skip => (),
            BorrowWaker::Ref(waker) => waker.wake(),
            BorrowWaker::Mut(waker) => waker.wake(),
        }
    }

    pub fn set(&mut self, cx: &mut std::task::Context<'_>) {
        match self {
            BorrowWaker::Skip => (),
            BorrowWaker::Ref(waker) | BorrowWaker::Mut(waker) => *waker = cx.waker().clone(),
        }
    }
}

// MARK: Raw Pointers

struct SharedPtr<T> {
    inner: NonNull<BorrowInner<T>>
}

unsafe impl<T> Send for SharedPtr<T> {}

unsafe impl<T> Sync for SharedPtr<T> {}

impl<T> Debug for SharedPtr<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::sync::PoisonError;
        let inner = self.inner();
        let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("SharedPtr")
            .field("weak", &inner.weak.load(Ordering::Relaxed))
            .field("strong", &state.strong)
            .field("mut_toggles", &state.mut_toggles)
            .field("stack", &state.stack)
            .field("data", &inner.data)
            .finish()
    }
}

impl<T> SharedPtr<T> {
    pub fn new(data: T) -> Self {
        SharedPtr {
            inner: unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(BorrowInner {
                weak: 1.into(),
                state: Mutex::new(BorrowState {
                    strong: 1,
                    mut_toggles: 0,
                    stack: Vec::new(),
                }),
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

#[derive(Debug)]
struct BorrowPtr<T> {
    shared: SharedPtr<T>
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

impl<T> Drop for BorrowPtr<T> {
    fn drop(&mut self) {
        let Ok(mut state) = self.inner().state.lock() else { return }; // TODO: handle poisoning
        state.strong -= 1;
        if state.is_mut() {
            state.mut_toggles += 1; // now immutable
            state.drain_empty();
        } else if state.strong == 0 {
            state.drain_empty();
        }
        if state.strong == 0 {
            state.mut_toggles += 1; // now mutable
            drop(state);
            unsafe { self.inner().data.get().as_mut().unwrap_unchecked().assume_init_drop() };
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

#[derive(Debug)]
struct First;

impl Index for First {
    fn get(&self) -> usize {
        0
    }
}

#[derive(Debug)]
struct FuturePtr<T, I: Index> {
    shared: Option<SharedPtr<T>>,
    index: I
}

impl<T, I: Index> FuturePtr<T, I> {
    pub fn polymorphic(mut self) -> FuturePtr<T, usize> {
        FuturePtr { shared: self.shared.take(), index: self.index.get() }
    }
} 

impl<T, I: Index> Future for FuturePtr<T, I> {
    type Output = SharedPtr<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let mut state = self.shared.as_ref().unwrap().inner().state.lock().unwrap();
        if let Some(waker) = state.stack.get_mut(self.index.get()) {
            waker.set(cx);
            return Poll::Pending
        };
        drop(state);
        Poll::Ready(unsafe { self.shared.take().unwrap_unchecked() })
    }
}

impl<T, I: Index> FusedFuture for FuturePtr<T, I> {
    fn is_terminated(&self) -> bool {
        self.shared.is_none()
    }
}

impl<T, I: Index> Drop for FuturePtr<T, I> {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else { return };
        let Ok(mut state) = shared.inner().state.lock() else { return }; // TODO: handle poisoning
        if let Some(waker) = state.stack.get_mut(self.index.get()) {
            *waker = BorrowWaker::Skip;
            drop(state);
            drop(shared)
        } else {
            drop(state);
            drop(BorrowPtr {
                shared,
            })
        }
    }
}

// MARK: Smart Pointers

#[derive(Debug)]
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
        let mut state = self.ptr.inner().state.lock().unwrap();
        state.mut_toggles += 1; // now immutable
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        drop(state);
        let ptr = self.into_shared();
        let shared = ptr.clone();
        (
            RefShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                _variance: PhantomData,
            },
            Ref {
                ptr: BorrowPtr { shared },
                _variance: PhantomData,
            }
        )
    }

    pub fn share_mut(self) -> (RefMutShare<T>, RefMut<T>) {
        let mut state = self.ptr.inner().state.lock().unwrap();
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        drop(state);
        let ptr = self.into_shared();
        let shared = ptr.clone();
        (
            RefMutShare {
                ptr: FuturePtr { shared: Some(ptr), index: First },
                _variance: PhantomData,
            },
            RefMut {
                ptr: BorrowPtr { shared },
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
        self.ptr.inner().state.lock().unwrap().mut_toggles += 1; // now immutable
        let shared = self.into_shared();
        Ref {
            ptr: BorrowPtr { shared },
            _variance: PhantomData
        }

    }

    pub fn into_mut(self) -> RefMut<T> {
        let shared = self.into_shared();
        RefMut {
            ptr: BorrowPtr { shared },
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

#[derive(Debug)]
pub struct Weak<T> {
    ptr: SharedPtr<T>,
    _variance: PhantomData<*const T>,
    mut_toggles: u64,
}

unsafe impl<T: Send> Send for Weak<T> {}

unsafe impl<T: Send> Sync for Weak<T> {}

impl<T> Weak<T> {
    pub fn upgrade(self) -> Option<Ref<T>> {
        let mut state = self.ptr.inner().state.lock().unwrap();
        if state.mut_toggles == self.mut_toggles {
            state.strong += 1;
            drop(state);
            Some(Ref {
                ptr: BorrowPtr { shared: self.ptr },
                _variance: PhantomData,
            })
        } else {
            None
        }
    }
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        Weak {
            ptr: self.ptr.clone(),
            _variance: self._variance,
            mut_toggles: self.mut_toggles
        }
    }
}

#[derive(Debug)]
pub struct Ref<T> {
    ptr: BorrowPtr<T>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for Ref<T> {}

unsafe impl<T: Send> Sync for Ref<T> {}

impl<T> Ref<T> {
    pub fn downgrade(&self) -> Weak<T> {
        let mut_toggles = self.ptr.inner().state.lock().unwrap().mut_toggles;
        Weak {
            ptr: self.ptr.clone(),
            _variance: PhantomData,
            mut_toggles,
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
        let strong = &mut self.ptr.shared.inner().state.lock().unwrap().strong;
        let shared = self.ptr.clone();
        *strong += 1;
        Ref {
            ptr: BorrowPtr { shared },
            _variance: PhantomData
        }
    }
}

#[derive(Debug)]
pub struct RefMut<T> {
    ptr: BorrowPtr<T>,
    _variance: PhantomData<*mut T>
}

unsafe impl<T: Send> Send for RefMut<T> {}

unsafe impl<T: Sync> Sync for RefMut<T> {}

impl<T> RefMut<T> {
    pub fn into_ref(self) -> Ref<T> {
        let mut state = self.ptr.inner().state.lock().unwrap();
        state.mut_toggles += 1; // now immutable
        state.drain_immutable();
        drop(state);
        Ref {
            ptr: self.ptr,
            _variance: PhantomData,
        }
    }

    pub fn forward_ref(self) -> (RefForward<T>, Ref<T>) {
        let mut state = self.ptr.inner().state.lock().unwrap();
        state.mut_toggles += 1; // now immutable
        let n = state.stack.len();
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        drop(state);
        let ptr = SharedPtr { inner: self.ptr.inner };
        std::mem::forget(self);
        let shared = ptr.clone();
        (
            RefForward {
                ptr: FuturePtr { shared: Some(ptr), index: n },
                _variance: PhantomData,
            },
            Ref {
                ptr: BorrowPtr { shared },
                _variance: PhantomData,
            }
        )
    }

    pub fn forward_mut(self) -> (RefMutForward<T>, RefMut<T>) {
        let mut state = self.ptr.inner().state.lock().unwrap();
        let n = state.stack.len();
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        drop(state);
        let ptr = SharedPtr { inner: self.ptr.inner };
        std::mem::forget(self);
        let shared = ptr.clone();
        (
            RefMutForward {
                ptr: FuturePtr { shared: Some(ptr), index: n },
                _variance: PhantomData,
            },
            RefMut {
                ptr: BorrowPtr { shared },
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

#[derive(Debug)]
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
        let mut state = self.ptr.shared.as_ref().unwrap().inner().state.lock().unwrap();
        let shared = unsafe { self.ptr.shared.as_ref().unwrap_unchecked() }.clone();
        if let Some(_) = state.stack.get_mut(self.ptr.index.get()) {
            state.strong += 1;
            return Ref {
                ptr: BorrowPtr { shared },
                _variance: PhantomData,
            }
        };
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        state.mut_toggles += 1; // now immutable
        return Ref {
            ptr: BorrowPtr { shared },
            _variance: PhantomData,
        }
    }

    pub fn into_ref(self) -> Ref<T> {
        self.into_forward().into_ref()
    }

    pub fn into_forward(self) -> RefForward<T> {
        RefForward { ptr: self.ptr.polymorphic(), _variance: PhantomData }
    }
}

impl<T> Future for RefShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_unpin(cx).map(|ptr| ShareBox { ptr, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefShare<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

#[derive(Debug)]
pub struct RefMutShare<T> {
    ptr: FuturePtr<T, First>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Send> Send for RefMutShare<T> {}

unsafe impl<T: Sync> Sync for RefMutShare<T> {}

impl<T> RefMutShare<T> {
    pub fn into_forward(self) -> RefMutForward<T> {
        RefMutForward { ptr: self.ptr.polymorphic(), _variance: PhantomData }
    }
}

impl<T> Future for RefMutShare<T> {
    type Output = ShareBox<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_unpin(cx).map(|ptr| ShareBox { ptr, _variance: PhantomData })
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

#[derive(Debug)]
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

    pub fn create_ref(&mut self) -> Ref<T> {
        let mut state = self.ptr.shared.as_ref().unwrap().inner().state.lock().unwrap();
        let shared = unsafe { self.ptr.shared.as_ref().unwrap_unchecked() }.clone();
        if let Some(_) = state.stack.get_mut(self.ptr.index.get()) {
            state.strong += 1;
            return Ref {
                ptr: BorrowPtr { shared },
                _variance: PhantomData,
            }
        };
        let n = state.stack.len();
        state.stack.push(BorrowWaker::Mut(futures::task::noop_waker()));
        state.mut_toggles += 1; // now immutable
        drop(state);
        self.ptr.index = n;
        return Ref {
            ptr: BorrowPtr { shared },
            _variance: PhantomData,
        }
    }

    pub fn into_ref(mut self) -> Ref<T> {
        let shared = self.ptr.shared.take().unwrap();
        let mut state = shared.inner().state.lock().unwrap();
        if let Some(waker) = state.stack.get_mut(self.ptr.index.get()) {
            *waker = BorrowWaker::Skip;
            state.strong += 1;
            drop(state);
            return Ref {
                ptr: BorrowPtr { shared },
                _variance: PhantomData,
            }
        };
        state.mut_toggles += 1; // now immutable
        drop(state);
        return Ref {
            ptr: BorrowPtr { shared },
            _variance: PhantomData,
        }
    }
}

impl<T> Future for RefForward<T> {
    type Output = RefMut<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_unpin(cx).map(|shared: SharedPtr<T>| RefMut { ptr: BorrowPtr { shared }, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefForward<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

#[derive(Debug)]
pub struct RefMutForward<T> {
    ptr: FuturePtr<T, usize>,
    _variance: PhantomData<*mut T>
}

unsafe impl<T: Send> Send for RefMutForward<T> {}

unsafe impl<T: Sync> Sync for RefMutForward<T> {}

impl<T> RefMutForward<T> {
    pub fn into_ref_blocking(self) -> RefBlocking<T> {
        let mut state = self.ptr.shared.as_ref().unwrap().inner().state.lock().unwrap();
        if let Some(waker) = state.stack.get_mut(self.ptr.index.get()) {
            match std::mem::replace(waker, BorrowWaker::Skip) {
                BorrowWaker::Skip => (),
                BorrowWaker::Ref(_) => unreachable!(),
                BorrowWaker::Mut(inner_waker) => {
                    *waker = BorrowWaker::Ref(inner_waker)
                },
            };
        } else {
            state.mut_toggles += 1; // now immutable
        }
        state.shake();
        drop(state);
        RefBlocking { ptr: self.ptr, _variance: PhantomData }
    }
}

impl<T> Future for RefMutForward<T> {
    type Output = RefMut<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_unpin(cx).map(|shared: SharedPtr<T>| RefMut { ptr: BorrowPtr { shared }, _variance: PhantomData })
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

#[derive(Debug)]
pub struct RefBlocking<T> {
    ptr: FuturePtr<T, usize>,
    _variance: PhantomData<*const T>
}

unsafe impl<T: Sync> Send for RefBlocking<T> {}

unsafe impl<T: Sync> Sync for RefBlocking<T> {}

impl<T> Future for RefBlocking<T> {
    type Output = Ref<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        self.ptr.poll_unpin(cx).map(|shared: SharedPtr<T>| Ref { ptr: BorrowPtr { shared }, _variance: PhantomData })
    }
}

impl<T> FusedFuture for RefBlocking<T> {
    fn is_terminated(&self) -> bool {
        self.ptr.is_terminated()
    }
}

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
                            sleep(Duration::from_secs(3)).await;
                            *a.lock().unwrap() += 1;
                        });
                        spawn(async move {
                            sleep(Duration::from_secs(2)).await;
                            *b.lock().unwrap() += 1;
                        });
                        spawn(async move {
                            sleep(Duration::from_secs(1)).await;
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
                    sleep(Duration::from_secs(3)).await;
                    *a.lock().unwrap() += 1;
                });
                spawn(async move {
                    sleep(Duration::from_secs(2)).await;
                    *b.lock().unwrap() += 1;
                });
                spawn(async move {
                    sleep(Duration::from_secs(1)).await;
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
        let visitor = example.ptr.clone();
        println!("new: {:?}", &visitor);
        drop(example);
        println!("drop: {:?}", &visitor);
    }

    #[test]
    fn new_take() {
        let example = ShareBox::new(());
        let visitor = example.ptr.clone();
        println!("new: {:?}", &visitor);
        example.into_inner();
        println!("into: {:?}", &visitor);
    }

    #[test]
    fn new_into_ref() {
        let example = ShareBox::new(());
        let visitor = example.ptr.clone();
        println!("new: {:?}", &visitor);
        drop(example.into_ref());
        println!("ref: {:?}", &visitor);
    }

    #[tokio::test]
    async fn lil_test() {
        let example = ShareBox::new(());
        let visitor = example.ptr.clone();
        println!("new: {:?}", &visitor);
        let (fut, rm) = example.share_mut();
        println!("shared: {:?}", &visitor);
        drop(rm);
        println!("dropped: {:?}", &visitor);
        let example = fut.await;
        println!("awaited: {:?}", &visitor);
        example.into_inner();
        println!("into: {:?}", &visitor);
    }
}