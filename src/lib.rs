use std::{borrow::{Borrow, BorrowMut}, cell::UnsafeCell, future::Future, marker::PhantomData, mem::MaybeUninit, ops::{Deref, DerefMut}, pin::Pin, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex}, usize};

use futures::{future::FusedFuture, FutureExt, Stream, StreamExt};

mod graph;
pub mod scope;
pub mod slice;
pub mod prelude;
pub mod tuple;

use graph::Graph;
use scope::{Anchor, Spawner};

// MARK: Inner

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

    pub fn into_raw(self) -> *mut T {
        let ptr = unsafe { self.inner.as_ptr().cast::<T>().byte_add(std::mem::offset_of!(BorrowInner<T>, data)) };
        std::mem::forget(self);
        ptr
    }

    pub unsafe fn from_raw(ptr: *mut T) -> Self {
        SharedPtr {
            inner: NonNull::new_unchecked(ptr.byte_sub(std::mem::offset_of!(BorrowInner<T>, data)).cast::<BorrowInner<T>>()),
        }
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
}

impl<T> FuturePtr<T, First> {
    pub fn poll_shared(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<SharedPtr<T>> {
        Pin::new(self).poll(cx).map(|(_, shared)| shared)
    }
}

impl<T> FuturePtr<T, usize> {
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
        let Some(shared) = self.shared.take() else { return };
        let mut graph = shared.inner().graph.lock().unwrap();
        if unsafe { graph.close_future_unchecked(self.index.get()) } {
            unsafe { shared.inner().data.get().as_mut().unwrap_unchecked().assume_init_drop() };
            drop(graph); // release mutex
            drop(shared)
        }
    }
}

// MARK: Black Magic

pub struct Context<'a, T> {
    ptr: BorrowPtr<T>,
    _contextualise_ref: PhantomData<fn(&'a T) -> Ref<T>>,
    _contextualise_mut: PhantomData<fn(&'a mut T) -> RefMut<T>>,
}

unsafe impl<'a, T: Send> Send for Context<'a, T> {}

unsafe impl<'a, T: Send> Sync for Context<'a, T> {}

impl<'a, T> Clone for Context<'a, T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr.clone(),
            _contextualise_ref: PhantomData,
            _contextualise_mut: PhantomData
        }
    }
}

impl<'a, T> Context<'a, T> {
    pub fn contextualise_ref<B: ?Sized>(self, rf: &'a B) -> Ref<T, B> {
        Ref {
            ptr: self.ptr,
            borrow: rf,
        }
    }

    pub fn contextualise_mut<B: ?Sized>(self, rf: &'a mut B) -> RefMut<T, B> {
        RefMut {
            ptr: self.ptr,
            borrow: rf,
        }
    }

    pub fn lift_ref<B: ?Sized>(&self, rf: &'a B) -> Ref<T, B> {
        self.clone().contextualise_ref(rf)
    }

    pub fn lift_mut<B: ?Sized>(&self, rf_mut: &'a mut B) -> RefMut<T, B> {
        self.clone().contextualise_mut(rf_mut)
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

    /// Converts self into a new ready `RefShare` future.
    pub fn into_ref_share(self) -> RefShare<T> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        debug_assert_eq!(graph.share(), 0, r#"
            Graph should only have one root and it must be the last to be freed,
            hence it should be at the start of the free list / stack
            "#
        );
        let index = 0;
        unsafe { graph.untrack_borrow_unchecked(index) };
        drop(graph);
        let ptr = self.into_shared();
        RefShare {
            ptr: FuturePtr { shared: Some(ptr), index: First },
            _borrow: PhantomData,
        }
    }

    /// Converts self into a new ready `RefMutShare` future.
    pub fn into_mut_share(self) -> RefMutShare<T> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        debug_assert_eq!(graph.share(), 0, r#"
            Graph should only have one root and it must be the last to be freed,
            hence it should be at the start of the free list / stack
            "#
        );
        let index = 0;
        unsafe { graph.untrack_borrow_unchecked(index) };
        drop(graph);
        let ptr = self.into_shared();
        RefMutShare {
            ptr: FuturePtr { shared: Some(ptr), index: First },
            _borrow: PhantomData,
        }
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
                _borrow: PhantomData,
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
                _borrow: PhantomData,
            },
            RefMut {
                ptr: BorrowPtr { shared, index },
                borrow,
            }
        )
    }

    pub fn spawn_ref<U>(self, f: impl FnOnce(Ref<T>) -> U) -> RefShare<T> {
        let (fut, rf) = self.share_ref();
        f(rf);
        fut
    }

    pub fn spawn_mut<U>(self, f: impl FnOnce(RefMut<T>) -> U) -> RefMutShare<T> {
        let (fut, rf_mut) = self.share_mut();
        f(rf_mut);
        fut
    }

    pub fn into_ref(self) -> Ref<T> {
        let index = 0;
        let shared = self.into_shared();
        let borrow = shared.inner().data.get() as *const T;
        Ref {
            ptr: BorrowPtr { shared, index },
            borrow,
        }

    }

    pub fn into_mut(self) -> RefMut<T> {
        let index = 0;
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

    pub fn into_raw(self) -> *const T {
        self.into_shared().into_raw()
    }

    pub unsafe fn from_raw(ptr: *const T) -> ShareBox<T> {
        ShareBox {
            ptr: SharedPtr::from_raw(ptr as *mut T),
            _share: PhantomData
        }
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
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn map<C: ?Sized>(self, f: impl for<'a> FnOnce(&'a B) -> &'a C) -> Ref<T, C> {
        Ref {
            ptr: self.ptr,
            borrow: f(unsafe { self.borrow.as_ref().unwrap_unchecked() }) as *const C
        }
    }

    /// Uses the same principle behind other scoping APIs to provide additional support to specific lifetimes.
    /// 
    /// The lifetime in question here is the same `'a` as referenced in `Ref::map`, as the true `'a` can not be know
    /// the lifetime `'_` will be used as a stand_in as it is truly anonymous.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn context<R>(self, f: impl for<'a> FnOnce(&'a B, Context<'a, T>) -> R) -> R {
        let Ref {
            ptr,
            borrow
        } = self;
        let context = Context::<'_, T> {
            ptr,
            _contextualise_ref: PhantomData,
            _contextualise_mut: PhantomData,
        };
        f(unsafe { borrow.as_ref().unwrap() }, context)
    }

    /// Uses the same principle behind other scoping APIs to provide additional support to specific lifetimes.
    /// 
    /// The lifetime in question here is the same `'a` as referenced in `Ref::map`, as the true `'a` can not be know
    /// the lifetime `'_` will be used as a stand_in as it is truly anonymous.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub async fn scope<F: Future>(self, f: impl for<'a> FnOnce(&'a B, Context<'a, T>, Spawner<'a>) -> F) -> F::Output {
        let Ref {
            ptr,
            borrow
        } = self;
        let context = Context::<'_, T> {
            ptr,
            _contextualise_ref: PhantomData,
            _contextualise_mut: PhantomData,
        };
        let anchor = Anchor::new();
        let fut = f(unsafe { borrow.as_ref().unwrap() }, context, anchor.spawner.clone());
        let ((), output) = futures::join!(anchor.stream().collect::<()>(), fut);
        output
    }

    /// A special case of `Ref::map` to handle dereferencing.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn into_deref(self) -> Ref<T, B::Target> where B: Deref {
        self.map(B::deref)
    }

    /// A special case of `Ref::map` to handle borrowing.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn into_borrow<C>(self) -> Ref<T, C> where B: Borrow<C> {
        self.map(B::borrow)
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

impl<T, B: ?Sized> From<RefMut<T, B>> for Ref<T, B> {
    fn from(value: RefMut<T, B>) -> Self {
        value.into_ref()
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

    /// Converts self into a new ready `RefForward` future.
    pub fn into_ref_forward(self) -> RefForward<T, B> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        unsafe { graph.untrack_borrow_unchecked(index) };
        drop(graph);
        let ptr = SharedPtr { inner: self.ptr.inner };
        let borrow = self.borrow;
        std::mem::forget(self.ptr);
        RefForward {
            ptr: FuturePtr { shared: Some(ptr), index },
            borrow,
        }
    }

    /// Converts self into a new ready `RefMutForward` future.
    pub fn into_mut_forward(self) -> RefMutForward<T, B> {
        let mut graph = self.ptr.inner().graph.lock().unwrap();
        let index = graph.forward(self.ptr.index);
        unsafe { graph.untrack_borrow_unchecked(index) };
        drop(graph);
        let ptr = SharedPtr { inner: self.ptr.inner };
        let borrow = self.borrow;
        std::mem::forget(self.ptr);
        RefMutForward {
            ptr: FuturePtr { shared: Some(ptr), index },
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
        let (fut, rf) = self.forward_ref();
        f(rf);
        fut
    }

    pub fn cleave_mut<U>(self, f: impl FnOnce(RefMut<T, B>) -> U) -> RefMutForward<T, B> {
        let (fut, rf_mut) = self.forward_mut();
        f(rf_mut);
        fut
    }

    /// Given that `f` works for all lifetimes `'a`, it will also work for
    /// the indeterminable, yet existant, lifetime of the `BorrowInner`.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn map<C: ?Sized>(self, f: impl for<'a> FnOnce(&'a mut B) -> &'a mut C) -> RefMut<T, C> {
        RefMut {
            ptr: self.ptr,
            borrow: f(unsafe { self.borrow.as_mut().unwrap_unchecked() }) as *mut C
        }
    }

    /// Uses the same principle behind other scoping APIs to provide additional support to specific lifetimes.
    /// 
    /// The lifetime in question here is the same `'a` as referenced in `RefMut::map`, as the true `'a` can not be know
    /// the lifetime `'_` will be used as a stand_in as it is truly anonymous.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn context<R>(self, f: impl for<'a> FnOnce(&'a mut B, Context<'a, T>) -> R) -> R {
        let RefMut {
            ptr,
            borrow
        } = self;
        let context = Context::<'_, T> {
            ptr,
            _contextualise_ref: PhantomData,
            _contextualise_mut: PhantomData,
        };
        f(unsafe { borrow.as_mut().unwrap() }, context)
    }

    /// Uses the same principle behind other scoping APIs to provide additional support to specific lifetimes.
    /// 
    /// The lifetime in question here is the same `'a` as referenced in `RefMut::map`, as the true `'a` can not be know
    /// the lifetime `'_` will be used as a stand_in as it is truly anonymous.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub async fn scope<F: Future>(self, f: impl for<'a> FnOnce(&'a mut B, Context<'a, T>, Spawner<'a>) -> F) -> F::Output {
        let RefMut {
            ptr,
            borrow
        } = self;
        let context = Context::<'_, T> {
            ptr,
            _contextualise_ref: PhantomData,
            _contextualise_mut: PhantomData,
        };
        let anchor = Anchor::new();
        let fut = f(unsafe { borrow.as_mut().unwrap() }, context, anchor.spawner.clone());
        let ((), output) = futures::join!(anchor.stream().collect::<()>(), fut);
        output
    }

    /// A special case of `RefMut::map` to handle dereferencing.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn into_deref_mut(self) -> RefMut<T, B::Target> where B: DerefMut {
        self.map(B::deref_mut)
    }

    /// A special case of `RefMut::map` to handle borrowing.
    /// 
    /// # Experimental
    /// 
    /// This is intended to be a safe API, and I believe that it is safe,
    /// however the work to prove this thoroughly is complex and has not yet been undertaken.
    /// This work will be done before v1.0.0.
    /// 
    /// > **Do not rely on this being safe in safety critical contexts**
    pub fn into_borrow_mut<C>(self) -> RefMut<T, C> where B: BorrowMut<C> {
        self.map(B::borrow_mut)
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
    _borrow: PhantomData<*const T>,
}

unsafe impl<T: Send + Sync> Send for RefShare<T> {}

unsafe impl<T: Send + Sync> Sync for RefShare<T> {}

impl<T> RefShare<T> {
    pub fn spawn(&mut self) -> SpawnRef<'_, T> {
        SpawnRef { fut: self }
    }

    pub fn spawn_while<F: Unpin + for<'a> FnMut(&'a mut T) -> bool>(&mut self, f: F) -> SpawnRefWhile<'_, T, F> {
        SpawnRefWhile { fut: self, f }
    }

    pub fn get(&self) -> Option<&T> {
        unsafe { self.ptr.shared.as_ref().map(|shared| shared.get_ref()) }
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

    pub fn into_raw(mut self) -> Option<*const T> {
        self.ptr.shared.take().map(|shared| shared.into_raw() as *const T)
    }

    pub unsafe fn from_raw(ptr: *const T) -> RefShare<T> {
        RefShare {
            ptr: FuturePtr {
                shared: Some(SharedPtr::from_raw(ptr as *mut T)),
                index: First
            },
            _borrow: PhantomData,
        }
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
    _borrow: PhantomData<*const T>,
}

unsafe impl<T: Send> Send for RefMutShare<T> {}

unsafe impl<T: Sync> Sync for RefMutShare<T> {}

impl<T> RefMutShare<T> {
    pub fn spawn_mut(&mut self) -> SpawnRefMut<'_, T> {
        SpawnRefMut { fut: self }
    }

    pub fn spawn_mut_while<F: Unpin + for<'a> FnMut(&'a mut T) -> bool>(&mut self, f: F) -> SpawnRefMutWhile<'_, T, F> {
        SpawnRefMutWhile { fut: self, f }
    }

    pub fn generalise(self) -> RefMutForward<T> {
        let borrow = self.ptr.shared.as_ref().unwrap().inner().data.get() as *mut T;
        RefMutForward { ptr: self.ptr.generalise(), borrow }
    }

    pub fn into_raw(mut self) -> Option<*const T> {
        self.ptr.shared.take().map(|shared| shared.into_raw() as *const T)
    }

    pub unsafe fn from_raw(ptr: *const T) -> RefMutShare<T> {
        RefMutShare {
            ptr: FuturePtr {
                shared: Some(SharedPtr::from_raw(ptr as *mut T)),
                index: First
            },
            _borrow: PhantomData,
        }
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
        RefMutShare { ptr: value.ptr, _borrow: value._borrow }
    }
}

pub struct RefForward<T, B: ?Sized = T> {
    ptr: FuturePtr<T, usize>,
    borrow: *mut B,
}

unsafe impl<T: Send, B: ?Sized + Sync> Send for RefForward<T, B> {}

unsafe impl<T: Send, B: ?Sized + Sync> Sync for RefForward<T, B> {}

impl<T, B: ?Sized> RefForward<T, B> {
    pub fn split(&mut self) -> SplitRef<'_, T, B> {
        SplitRef { fut: self }
    }

    pub fn split_while<F: Unpin + for<'a> FnMut(&'a mut B) -> bool>(&mut self, f: F) -> SplitRefWhile<'_, T, B, F> {
        SplitRefWhile { fut: self, f }
    }

    pub fn get(&self) -> Option<&T> {
        unsafe { self.ptr.shared.as_ref().map(|shared| shared.get_ref()) }
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

impl<T, B: ?Sized> RefMutForward<T, B> {
    pub fn split_mut(&mut self) -> SplitRefMut<'_, T, B> {
        SplitRefMut { fut: self }
    }

    pub fn split_mut_while<F: Unpin + for<'a> FnMut(&'a mut B) -> bool>(&mut self, f: F) -> SplitRefMutWhile<'_, T, B, F> {
        SplitRefMutWhile { fut: self, f }
    }
}

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

pub struct SpawnRef<'f, T> {
    fut: &'f mut RefShare<T>
}

impl<'f, T> Future for SpawnRef<'f, T> {
    type Output = Ref<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(owner) => {
                let (fut, rf) = owner.share_ref();
                *self.fut = fut;
                Poll::Ready(rf)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T> Deref for SpawnRef<'f, T> {
    type Target = RefShare<T>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T> DerefMut for SpawnRef<'f, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SpawnRefMut<'f, T> {
    fut: &'f mut RefMutShare<T>
}

impl<'f, T> Future for SpawnRefMut<'f, T> {
    type Output = RefMut<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(owner) => {
                let (fut, rf_mut) = owner.share_mut();
                *self.fut = fut;
                Poll::Ready(rf_mut)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T> Deref for SpawnRefMut<'f, T> {
    type Target = RefMutShare<T>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T> DerefMut for SpawnRefMut<'f, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SplitRef<'f, T, B: ?Sized> {
    fut: &'f mut RefForward<T, B>
}

impl<'f, T, B: ?Sized> Future for SplitRef<'f, T, B> {
    type Output = Ref<T, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(rf_mut) => {
                let (fut, rf) = rf_mut.forward_ref();
                *self.fut = fut;
                Poll::Ready(rf)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, B: ?Sized> Deref for SplitRef<'f, T, B> {
    type Target = RefForward<T, B>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, B: ?Sized> DerefMut for SplitRef<'f, T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SplitRefMut<'f, T, B: ?Sized> {
    fut: &'f mut RefMutForward<T, B>
}

impl<'f, T, B: ?Sized> Future for SplitRefMut<'f, T, B> {
    type Output = RefMut<T, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(rf_mut) => {
                let (fut, rf_mut) = rf_mut.forward_mut();
                *self.fut = fut;
                Poll::Ready(rf_mut)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, B: ?Sized> Deref for SplitRefMut<'f, T, B> {
    type Target = RefMutForward<T, B>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, B: ?Sized> DerefMut for SplitRefMut<'f, T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

// MARK: Streams

pub struct SpawnRefWhile<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> {
    fut: &'f mut RefShare<T>,
    f: F
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> Stream for SpawnRefWhile<'f, T, F> {
    type Item = Ref<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(mut owner) => {
                if (self.f)(&mut owner) {
                    let (fut, rf) = owner.share_ref();
                    *self.fut = fut;
                    Poll::Ready(Some(rf))
                } else {
                    *self.fut = owner.into_ref_share();
                    Poll::Ready(None)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> Deref for SpawnRefWhile<'f, T, F> {
    type Target = RefShare<T>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> DerefMut for SpawnRefWhile<'f, T, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SpawnRefMutWhile<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> {
    fut: &'f mut RefMutShare<T>,
    f: F
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> Stream for SpawnRefMutWhile<'f, T, F> {
    type Item = RefMut<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(mut owner) => {
                if (self.f)(&mut owner) {
                    let (fut, rf_mut) = owner.share_mut();
                    *self.fut = fut;
                    Poll::Ready(Some(rf_mut))
                } else {
                    *self.fut = owner.into_mut_share();
                    Poll::Ready(None)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> Deref for SpawnRefMutWhile<'f, T, F> {
    type Target = RefMutShare<T>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, F: Unpin + for<'a> FnMut(&'a mut T) -> bool> DerefMut for SpawnRefMutWhile<'f, T, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SplitRefWhile<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> {
    fut: &'f mut RefForward<T, B>,
    f: F
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> Stream for SplitRefWhile<'f, T, B, F> {
    type Item = Ref<T, B>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(mut rf_mut) => {
                if (self.f)(&mut rf_mut) {
                    let (fut, rf) = rf_mut.forward_ref();
                    *self.fut = fut;
                    Poll::Ready(Some(rf))
                } else {
                    *self.fut = rf_mut.into_ref_forward();
                    Poll::Ready(None)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> Deref for SplitRefWhile<'f, T, B, F> {
    type Target = RefForward<T, B>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> DerefMut for SplitRefWhile<'f, T, B, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

pub struct SplitRefMutWhile<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> {
    fut: &'f mut RefMutForward<T, B>,
    f: F
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> Stream for SplitRefMutWhile<'f, T, B, F> {
    type Item = RefMut<T, B>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match self.fut.poll_unpin(cx) {
            Poll::Ready(mut rf_mut) => {
                if (self.f)(&mut rf_mut) {
                    let (fut, rf) = rf_mut.forward_mut();
                    *self.fut = fut;
                    Poll::Ready(Some(rf))
                } else {
                    *self.fut = rf_mut.into_mut_forward();
                    Poll::Ready(None)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> Deref for SplitRefMutWhile<'f, T, B, F> {
    type Target = RefMutForward<T, B>;

    fn deref(&self) -> &Self::Target {
        self.fut
    }
}

impl<'f, T, B: ?Sized, F: Unpin + for<'a> FnMut(&'a mut B) -> bool> DerefMut for SplitRefMutWhile<'f, T, B, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fut
    }
}

// MARK: Wide

#[allow(type_alias_bounds)]
pub type WideShareBox<T: ?Sized> = ShareBox<Box<T>>;

impl<T: ?Sized> WideShareBox<T> {
    pub fn share_wide_ref(self) -> (WideRefShare<T>, WideRef<T>) {
        let (fut, rf) = self.share_ref();
        (fut, rf.into_deref())
    }

    pub fn share_wide_mut(self) -> (WideRefMutShare<T>, WideRefMut<T>) {
        let (fut, rf_mut) = self.share_mut();
        (fut, rf_mut.into_deref_mut())
    }

    pub fn spawn_wide_ref<U>(self, f: impl FnOnce(WideRef<T>) -> U) -> WideRefShare<T> {
        self.spawn_ref(|rf| f(rf.into_deref()))
    }

    pub fn spawn_wide_mut<U>(self, f: impl FnOnce(WideRefMut<T>) -> U) -> WideRefMutShare<T> {
        self.spawn_mut(|rf_mut| f(rf_mut.into_deref_mut()))
    }
}

#[allow(type_alias_bounds)]
pub type WideWeak<T: ?Sized, B: ?Sized = T> = Weak<Box<T>, B>;

#[allow(type_alias_bounds)]
pub type WideRef<T: ?Sized, B: ?Sized = T> = Ref<Box<T>, B>;

#[allow(type_alias_bounds)]
pub type WideRefMut<T: ?Sized, B: ?Sized = T> = RefMut<Box<T>, B>;

#[allow(type_alias_bounds)]
pub type WideRefShare<T: ?Sized> = RefShare<Box<T>>;

impl<T: ?Sized> WideRefShare<T> {
    pub fn as_wide_ref(&self) -> WideRef<T> {
        self.as_ref().into_deref()
    }
}

#[allow(type_alias_bounds)]
pub type WideRefMutShare<T: ?Sized> = RefMutShare<Box<T>>;

#[allow(type_alias_bounds)]
pub type WideRefForward<T: ?Sized, B: ?Sized = T> = RefForward<Box<T>, B>;

#[allow(type_alias_bounds)]
pub type WideRefMutForward<T: ?Sized, B: ?Sized = T> = RefMutForward<Box<T>, B>;

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

    #[tokio::test]
    async fn context_test() {
        use futures::FutureExt;
        let x =
            ShareBox::new((0_i8, 0_i8))
            .spawn_mut(|rf_mut| tokio::spawn(async move {
                rf_mut
                    .cleave_mut(|rf_mut| {
                        let (mut rf_mut_left, mut rf_mut_right) = rf_mut.context(|(a, b), context| {
                            (context.lift_mut(a), context.lift_mut(b))
                        });
                        tokio::spawn(async move {
                            *rf_mut_left += 1
                        });
                        tokio::spawn(async move {
                            *rf_mut_right -= 1
                        });
                    })
                    .map(|mut rf_mut| {
                        let ab = &mut* rf_mut;
                        std::mem::swap(&mut ab.0, &mut ab.1)
                    })
                    .await
            }))
            .await
            .into_inner();
            assert_eq!(x, (-1_i8, 1_i8));
    }

    #[tokio::test]
    async fn test_into_mut_share() {
        ShareBox::new(())
            .into_mut_share()
            .await
            .into_inner()
    }

    #[tokio::test]
    async fn test_into_mut_forward() {
        ShareBox::new(())
            .spawn_mut(|rf_mut| tokio::spawn(async move {
                rf_mut
                    .into_mut_forward()
                    .await
            }))
            .await
            .into_inner()
    }

    #[tokio::test]
    async fn test_drop_forward_then_drop_future() {
        ShareBox::new(())
            .spawn_mut(|rf_mut| tokio::spawn(async move {
                let (fut, rf_mut) = rf_mut
                    .forward_mut();
                drop(rf_mut);
                drop(fut);
            }))
            .await
            .into_inner()
    }

    #[tokio::test]
    async fn test_drop_future_then_drop_forward() {
        ShareBox::new(())
            .spawn_mut(|rf_mut| tokio::spawn(async move {
                let (fut, rf_mut) = rf_mut
                    .forward_mut();
                drop(fut);
                drop(rf_mut);
            }))
            .await
            .into_inner()
    }
}