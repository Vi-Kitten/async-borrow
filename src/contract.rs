use std::{cell::UnsafeCell, mem::MaybeUninit, ops::{Deref, DerefMut}, pin::Pin, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex, PoisonError}, task::{Context, Poll, Waker}};

mod borrow_queue;
pub mod states;

use states::{Owner, PtrState, RefState};
use borrow_queue::{BorrowId, BorrowKind, BorrowQueue};

// MARK: Contract

struct Contract<T> {
    /// The number of refereces pointing to the contract (includes weak references and owner).
    pub ref_count: AtomicUsize,
    /// Stores the shared data.
    pub value: UnsafeCell<MaybeUninit<T>>,
    /// Synchronization information.
    pub contract_state: Mutex<ContractState>,
}

pub struct ContractState {
    /// Contains important counts
    counts: ContractCounts,
    /// Contains queued borrows
    wakers: BorrowQueue,
}

impl ContractState {
    pub fn new() -> Self {
        ContractState { counts: ContractCounts::new(), wakers: BorrowQueue::new() }
    }

    pub fn reduce_borrow(&mut self, from: &BorrowKind) -> AdditionalWork {
        if self.counts.borrow_count == 0 {
            self.wake_from(BorrowKind::Empty)
        } else if from.is_exotic() && self.counts.exotic_count == 0 {
            self.wake_from(BorrowKind::Ref)
        } else {
            AdditionalWork::Noop
        }
    }

    /// Casts the borrow and handles waking
    pub fn cast_borrow(&mut self, from: &BorrowKind, to: &BorrowKind) {
        self.counts.record_borrow_release(from);
        self.counts.record_borrow_acquire(to);
        if to < from {
            // not the last time a wake will happen, no additional cleanup required.
            let _ = self.reduce_borrow(from);
        }
    }

    /// Casts the borrow without handling waking
    pub fn silent_cast_borrow(&mut self, from: &BorrowKind, to: &BorrowKind) {
        self.counts.record_borrow_release(from);
        self.counts.record_borrow_acquire(to);
    }

    pub fn wake_from(&mut self, kind: BorrowKind) -> AdditionalWork {
        self.wakers.wake_from(kind, &mut self.counts)
    }

    pub fn poll_reacquire(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self.wakers.owner_waker().as_mut() {
            Some(waker) => {
                *waker = cx.waker().clone();
                Poll::Pending
            },
            None => Poll::Ready(()),
        }
    }

    pub fn cancel_reaquire(&mut self) -> Option<()> {
        self.wakers.owner_waker().take().map(drop)
    }
}

pub struct ContractCounts {
    /// The number of exotic references that could upgrade to become mutable (or are already mutable).
    /// 
    /// This is important for ensuring strict borrow semantics.
    exotic_count: usize,
    /// The number of strong references held.
    borrow_count: usize,
    /// Number of mutable borrows thusfar (counts upgrading, owner re-aquisition and dropping).
    mut_number: u64,
}

impl ContractCounts {
    pub fn new() -> Self {
        ContractCounts { exotic_count: 0, borrow_count: 0, mut_number: 0 }
    }

    pub fn record_borrow_acquire(&mut self, kind: &BorrowKind) {
        match kind {
            BorrowKind::Mut => {
                self.exotic_count += 1;
                self.borrow_count += 1;
                self.mut_number += 1;
            },
            BorrowKind::Upgradable => {
                self.exotic_count += 1;
                self.borrow_count += 1;
            }
            BorrowKind::Ref => {
                self.exotic_count += 1
            },
            BorrowKind::Empty => (),
        }
    }

    pub fn record_borrow_release(&mut self, kind: &BorrowKind) {
        match kind {
            BorrowKind::Mut | BorrowKind::Upgradable => {
                self.exotic_count -= 1;
                self.borrow_count -= 1;
            },
            BorrowKind::Ref => {
                self.exotic_count -= 1
            },
            BorrowKind::Empty => (),
        }
    }
}

// MARK: Pointers

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditionalWork {
    Noop,
    Drop,
}

pub struct Pointer<T, S: states::PtrState> {
    /// only valid to be uninit if about to forget
    inner: MaybeUninit<PointerInner<T, S>>
}

pub struct PointerInner<T, S: states::PtrState> {
    state: S,
    contract: NonNull<Contract<T>>,
}

impl<T, S: states::PtrState> Deref for Pointer<T, S> {
    type Target = PointerInner<T, S>;

    fn deref(&self) -> &Self::Target {
        unsafe { self.inner.assume_init_ref() }
    }
}

impl<T, S: states::PtrState> DerefMut for Pointer<T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.inner.assume_init_mut() }
    }
}

impl<T, S: states::PtrState> Pointer<T, S> {
    fn into_inner(mut self) -> PointerInner<T, S> {
        let inner = unsafe { std::mem::replace(&mut self.inner, MaybeUninit::uninit()).assume_init() };
        std::mem::forget(self);
        inner
    }

    fn handle_drop(inner: PointerInner<T, S>) -> Self {
        Pointer {
            inner: MaybeUninit::new(inner)
        }
    }

    fn new_ptr<R: states::PtrState>(&self, state: R) -> Pointer<T, R> {
        self.contract().ref_count.fetch_add(1, Ordering::Relaxed);
        Pointer::handle_drop(PointerInner { state, contract: self.contract })
    }
}

impl<T, S: PtrState> Pointer<T, S> {
    pub unsafe fn get_ref_unchecked(&self) -> &T {
        self.contract().value.get().as_ref().unwrap_unchecked().assume_init_ref()
    }

    pub unsafe fn get_mut_unchecked(&mut self) -> &mut T {
        self.contract().value.get().as_mut().unwrap_unchecked().assume_init_mut()
    }
}

impl<T, S: states::RefState> Pointer<T, S> {
    pub fn get_ref(&self) -> &T {
        unsafe { self.get_ref_unchecked() }
    }
}

impl<T, S: states::RefMutState> Pointer<T, S> {
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { self.get_mut_unchecked() }
    }
}

impl<T> Pointer<T, states::Weak> {
    fn upgrade_inner(self) -> Result<Pointer<T, states::Ref>, Pointer<T, states::Empty>> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        match state.try_upgrade(contract_state.counts.mut_number) {
            Ok(state) => {
                contract_state.cast_borrow(&BorrowKind::Empty, &states::Ref::BORROW_KIND);
                Ok(Pointer::handle_drop(PointerInner { state, contract }))
            },
            Err(state) => Err(Pointer::handle_drop(PointerInner { state, contract })),
        }
    }

    pub fn upgrade(self) -> Option<Pointer<T, states::Ref>> {
        self.upgrade_inner().ok()
    }
}

impl<T> Pointer<T, states::Ref> {
    pub fn downgrade(&self) -> Pointer<T, states::Weak> {
        // TODO: handle panics
        let mut_number = self.contract().contract_state.lock().unwrap().counts.mut_number;
        self.new_ptr(states::Weak(mut_number))
    }
}

impl<T> Pointer<T, states::UpgradableRef> {
    pub fn as_ref(&self) -> Pointer<T, states::Ref> {
        // TODO: handle panics
        self.contract().contract_state.lock().unwrap().counts.record_borrow_acquire(&states::Ref::BORROW_KIND);
        let ref_state = self.state.clone().into_ref();
        self.new_ptr(ref_state)
    }
}

impl<T> Pointer<T, states::RefMut> {
    pub fn into_ref(self) -> Pointer<T, states::Ref> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        contract_state.cast_borrow(&states::RefMut::BORROW_KIND, &states::Ref::BORROW_KIND);
        drop(contract_state);
        Pointer::handle_drop(PointerInner { state: state.into_ref(), contract })
    }

    pub fn downgrade(self) -> Pointer<T, states::UpgradableRef> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        // this cast will never prompt and wakes as future ptrs expect all potential mutation to cease until waking, including by upgradable refs
        contract_state.silent_cast_borrow(&states::RefMut::BORROW_KIND, &states::UpgradableRef::BORROW_KIND);
        drop(contract_state);
        Pointer::handle_drop(PointerInner { state: state.downgrade(), contract })
    }
}

impl<T, S: states::PtrState> Drop for Pointer<T, S> {
    fn drop(&mut self) {
        let PointerInner { state, contract } = unsafe { std::mem::replace(&mut self.inner, MaybeUninit::uninit()).assume_init() };
        match state.un_track(&unsafe { contract.as_ref() }.contract_state) {
            AdditionalWork::Noop => (),
            AdditionalWork::Drop => unsafe { contract.as_ref().value.get().as_mut().unwrap_unchecked().assume_init_drop() },
        }
        if unsafe { contract.as_ref() }.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            drop(unsafe { Box::from_raw(contract.as_ptr()) })
        }
    }
}

impl<T, S: states::PtrState> PointerInner<T, S> {
    fn contract(&self) -> &Contract<T> {
        unsafe { self.contract.as_ref() }
    }

    pub fn get_ref(&self) -> &T where S: states::RefState {
        unsafe { self.contract().value.get().as_ref().unwrap_unchecked().assume_init_ref() }
    }

    pub fn get_mut(&mut self) -> &mut T where S: states::RefMutState {
        unsafe { self.contract().value.get().as_mut().unwrap_unchecked().assume_init_mut() }
    }

    fn map_state<Q: states::PtrState>(self, f: impl FnOnce(S) -> Q) -> PointerInner<T, Q> {
        PointerInner {
            state: (f)(self.state),
            contract: self.contract
        }
    }
}

impl<T> Pointer<T, states::Owner> {
    pub fn init_with(value: T) -> Self {
        let contract = Contract {
            ref_count: 1.into(),
            value: UnsafeCell::new(MaybeUninit::new(value)),
            contract_state: Mutex::new(ContractState::new()),
        };
        let contract = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(contract))) };
        let state = unsafe { Owner::init() };
        Pointer::handle_drop(PointerInner { state, contract })
    }

    pub fn take_value(self) -> T {
        let value = unsafe { std::mem::replace(self.contract().value.get().as_mut().unwrap_unchecked(), MaybeUninit::uninit()).assume_init() };
        drop(Pointer::handle_drop(self.into_inner().map_state(|_| states::Empty)));
        value
    }
}

// MARK: Reacquire

pub struct ReacquisitionFuture<T> {
    host: Option<Pointer<T, states::Host>>,
}

impl<T> ReacquisitionFuture<T> {
    pub fn cancel(mut self) -> Pointer<T, states::Host> {
        self.host.take().unwrap()
    }
}

impl<T> futures::Future for ReacquisitionFuture<T> {
    type Output = Pointer<T, states::Owner>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let host = self.host.as_mut().unwrap();
        let mut contract = host.contract().contract_state.lock().unwrap();
        let poll = contract.poll_reacquire(cx);
        drop(contract);
        poll.map(|_| {
            // this is valid because we know that host is Some
            Pointer::handle_drop(unsafe { self.host.take().unwrap_unchecked().into_inner().map_state(|s| s.assert_unshared() ) })
        })
    }
}

impl<T> Pointer<T, states::Host> {
    pub fn acquire(self: Self) -> ReacquisitionFuture<T> {
        ReacquisitionFuture {
            host: Some(self)
        }
    } 
}

// MARK: Future Borrow

struct FuturePointer<T, S: states::PtrState> {
    id: BorrowId,
    inner: Option<(PointerInner<T, S>, std::sync::Arc<Mutex<borrow_queue::BorrowQueueNode>>)>
}

impl<T, S: states::PtrState> FuturePointer<T, S> {
    pub fn is_pollable(&self) -> bool {
        self.inner.is_some()
    }

    pub fn poll_val(&mut self, cx: &mut Context) -> Poll<Pointer<T, S>> {
        // TODO: handle panics
        let (pointer, queue) = self.inner.as_mut().unwrap();
        let mut node = queue.lock().unwrap();
        let poll = node.poll_at(&self.id, cx);
        drop(node);
        poll.map(|_| {
            let _ = pointer;
            // safe as we would have already panicked if this was not the case
            Pointer::handle_drop(unsafe { self.inner.take().unwrap_unchecked() }.0)
        })
    }
}

impl<T, S: states::PtrState> Drop for FuturePointer<T, S> {
    fn drop(&mut self) {
        let Some((pointer, queue)) = self.inner.take() else {
            return;
        };
        // TODO: handle panic
        let mut node = queue.lock().unwrap();
        if let Some(()) = node.close_waker(&self.id) {
            drop(Pointer::handle_drop(pointer.map_state(|_| states::Empty)));
            return;
        } 
        drop(node);
        drop(Pointer::handle_drop(pointer))
    }
}

// MARK: Tests

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vibe_check() {
        // ...
    }
}