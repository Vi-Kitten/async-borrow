use std::{cell::UnsafeCell, future::Future, marker::PhantomData, mem::MaybeUninit, ops::{Deref, DerefMut}, pin::Pin, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex}, task::Waker, usize};

use futures::{future::FusedFuture, FutureExt};

pub(crate) mod contract;
pub mod scope;
pub mod collections;

// MARK: Inner

struct BorrowInner<T> {
    weak: AtomicUsize,
    state: Mutex<BorrowState>,
    data: UnsafeCell<MaybeUninit<T>>
}

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

    pub fn poll(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        use std::task::Poll;
        match self {
            BorrowWaker::Skip => Poll::Ready(()),
            BorrowWaker::Ref(waker) | BorrowWaker::Mut(waker) => {
                *waker = cx.waker().clone();
                Poll::Pending
            },
        }
    }
}

// MARK: Raw Pointers

struct SharedPtr<T> {
    inner: NonNull<BorrowInner<T>>
}

impl<T> SharedPtr<T> {
    pub fn new(data: T) -> Self {
        SharedPtr {
            inner: unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(BorrowInner {
                weak: 0.into(),
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

    pub unsafe fn inner_mut(&mut self) -> &mut BorrowInner<T> {
        unsafe { self.inner.as_mut() }
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
            waker.poll(cx);
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

    pub fn spawn_ref(self, f: impl FnOnce(Ref<T>)) -> RefShare<T> {
        let (fut, ptr) = self.share_ref();
        f(ptr);
        fut
    }

    pub fn spawn_mut(self, f: impl FnOnce(RefMut<T>)) -> RefMutShare<T> {
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
        unsafe { self.into_shared().inner_mut().data.get_mut().assume_init_read() }
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
        unsafe { self.ptr.inner_mut().data.get_mut().assume_init_drop() }
    }
}

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

    fn into_shared(self) -> SharedPtr<T> {
        let ptr = SharedPtr { inner: self.ptr.inner };
        std::mem::forget(self);
        ptr
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

    pub fn cleave_ref(self, f: impl FnOnce(Ref<T>)) -> RefForward<T> {
        let (fut, ptr) = self.forward_ref();
        f(ptr);
        fut
    }

    pub fn cleave_mut(self, f: impl FnOnce(RefMut<T>)) -> RefMutForward<T> {
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