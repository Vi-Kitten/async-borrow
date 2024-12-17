use std::{cell::UnsafeCell, mem::MaybeUninit, ops::{Deref, DerefMut}, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex, PoisonError}, task::{Context, Poll, Waker}};

use states::Owner;

struct Contract<T> {
    /// The number of refereces pointing to the contract (includes weak references and owner).
    ref_count: AtomicUsize,
    /// Stores the shared data.
    value: UnsafeCell<MaybeUninit<T>>,
    /// Synchronization information.
    contract_state: Mutex<ContractState>,
}

struct ContractState {
    /// The number of exotic references that could upgrade to become mutable (or are already mutable).
    exotic_count: usize,
    /// The number of strong references held.
    borrow_count: usize,
    /// Number of mutable borrows thusfar (counts upgrading, owner re-aquisition and dropping).
    mut_number: usize,
    /// Contains queued borrows
    wakers: BorrowStack,
}

pub enum AdditionalWork {
    Nope,
    Drop,
}

struct CountDelta {
    exotic_count_offset: usize,
    borrow_count_offset: usize,
}

impl ContractState {
    pub fn new() -> Self {
        ContractState { exotic_count: 1, borrow_count: 1, mut_number: 0, wakers: BorrowStack::new() }
    }

    pub fn record_borrow_acquire(&mut self, kind: stack::BorrowKind) {
        match kind {
            stack::BorrowKind::Mut => {
                self.exotic_count += 1;
                self.borrow_count += 1;
                self.mut_number += 1;
            },
            stack::BorrowKind::Exotic => {
                self.exotic_count += 1;
                self.borrow_count += 1;
            }
            stack::BorrowKind::Ref => {
                self.exotic_count += 1
            },
            stack::BorrowKind::Skip => (),
        }
    }

    pub fn record_borrow_release(&mut self, kind: stack::BorrowKind) {
        match kind {
            stack::BorrowKind::Mut | stack::BorrowKind::Exotic => {
                self.exotic_count -= 1;
                self.borrow_count -= 1;
            },
            stack::BorrowKind::Ref => {
                self.exotic_count -= 1
            },
            stack::BorrowKind::Skip => (),
        }
    }

    fn handle_wake_info(&mut self, info: &WakeInfo) {
        self.exotic_count += info.exotics_woken;
        self.borrow_count += info.borrows_woken;
        if let stack::BorrowKind::Mut = info.final_wake {
            self.mut_number += 1;
        }
    }

    pub fn wake_from_inexotic(&mut self) {
        let info = self.wakers.wake_from_inexotic();
        self.handle_wake_info(&info);
    }

    pub fn wake_from_unborrowed(&mut self) -> AdditionalWork {
        let info = self.wakers.wake_from_inexotic();
        self.handle_wake_info(&info);
        match info.final_wake {
            stack::BorrowKind::Mut | stack::BorrowKind::Exotic | stack::BorrowKind::Ref => AdditionalWork::Nope,
            stack::BorrowKind::Skip => AdditionalWork::Drop,
        }
    }
}

mod stack {
    use super::*;

    pub struct BorrowId {
        index: usize,
        id: usize,
    }

    #[derive(Clone, Copy)]
    pub enum BorrowKind {
        Mut,
        Exotic,
        Ref,
        Skip,
    }

    struct BorrowWaker {
        kind: BorrowKind,
        waker: Waker,
        id: usize,
    }

    impl BorrowWaker {
        pub fn wake(self) {
            self.waker.wake();
        }
    }

    pub struct BorrowStack {
        next_id: usize,
        upgrade_buffer: Vec<BorrowWaker>,
        stack: Vec<BorrowWaker>,
    }

    pub struct WakeInfo {
        pub final_wake: BorrowKind,
        pub exotics_woken: usize,
        pub borrows_woken: usize,
    }

    impl BorrowStack {
        pub fn new() -> Self {
            BorrowStack {
                next_id: 0,
                upgrade_buffer: vec![],
                stack: vec![]
            }
        }

        pub fn reset(&mut self) {
            self.next_id = 0;
            self.upgrade_buffer.drain(0..).for_each(std::mem::forget);
            self.stack.drain(0..).for_each(std::mem::forget);
        }

        /// call when last upgradable reference is downgraded
        pub fn flush_upgrade_buffer(&mut self) {
            self.upgrade_buffer.reverse();
            self.stack.append(&mut self.upgrade_buffer);
        }

        fn get_ref(&self, index: usize) -> Option<&BorrowWaker> {
            if index < self.stack.len() {
                Some(unsafe { self.stack.get_unchecked(index) })
            } else {
                if let Some(index) = self.upgrade_buffer.len().checked_sub(index - self.stack.len()) {
                    Some(unsafe { self.upgrade_buffer.get_unchecked(index) })
                } else {
                    None
                }
            }
        }

        fn get_mut(&mut self, index: usize) -> Option<&mut BorrowWaker> {
            if index < self.stack.len() {
                Some(unsafe { self.stack.get_unchecked_mut(index) })
            } else {
                if let Some(index) = self.upgrade_buffer.len().checked_sub(index - self.stack.len()) {
                    Some(unsafe { self.upgrade_buffer.get_unchecked_mut(index) })
                } else {
                    None
                }
            }
        }

        fn get_ref_by_id(&self, id: &BorrowId) -> Option<&BorrowWaker> {
            let waker = self.get_ref(id.index)?;
            (waker.id == id.id).then_some(())?;
            Some(waker)
        }

        fn get_mut_by_id(&mut self, id: &BorrowId) -> Option<&mut BorrowWaker> {
            let waker = self.get_mut(id.index)?;
            (waker.id == id.id).then_some(())?;
            Some(waker)
        }

        pub fn poll_at(&mut self, id: &BorrowId, cx: &mut Context) -> Poll<()> {
            let Some(waker) = self.get_mut_by_id(id) else {
                return Poll::Ready(())
            };
            waker.waker = cx.waker().clone();
            Poll::Pending
        }

        pub fn close(&mut self, id: &BorrowId) -> Result<(), ()> {
            let Some(waker) = self.get_mut_by_id(id) else {
                return Err(())
            };
            waker.kind = BorrowKind::Skip;
            Ok(())
        }

        /// Expects buffer to be flushed
        pub fn wake_from_inexotic(&mut self) -> WakeInfo {
            let mut wake_info = WakeInfo {
                final_wake: BorrowKind::Skip,
                exotics_woken: 0,
                borrows_woken: 0,
            };
            'inexotic: while let Some(last) = self.stack.last() {
                match last.kind {
                    BorrowKind::Mut => break 'inexotic,
                    kind@BorrowKind::Exotic => {
                        unsafe { self.stack.pop().unwrap_unchecked().wake() };
                        wake_info.borrows_woken += 1;
                        wake_info.exotics_woken += 1;
                        wake_info.final_wake = kind;
                        break 'inexotic;
                    },
                    kind@BorrowKind::Ref => {
                        unsafe { self.stack.pop().unwrap_unchecked().wake() };
                        wake_info.borrows_woken += 1;
                        wake_info.final_wake = kind;
                        continue 'inexotic;
                    },
                    BorrowKind::Skip => {
                        unsafe { drop(self.stack.pop().unwrap_unchecked()) }
                        continue 'inexotic;
                    },
                }
                #[allow(unreachable_code)]
                {
                    unreachable!()
                }
            }
            wake_info
        }

        pub fn wake_from_unborrowed(&mut self) -> WakeInfo {
            let mut wake_info = WakeInfo {
                final_wake: BorrowKind::Skip,
                exotics_woken: 0,
                borrows_woken: 0,
            };
            'unborrowed: while let Some(last) = self.stack.last() {
                match last.kind {
                    kind@BorrowKind::Mut => {
                        unsafe { self.stack.pop().unwrap_unchecked().wake() };
                        wake_info.borrows_woken += 1;
                        wake_info.exotics_woken += 1;
                        wake_info.final_wake = kind;
                        break 'unborrowed;
                    }
                    kind@BorrowKind::Exotic => {
                        unsafe { self.stack.pop().unwrap_unchecked().wake() };
                        wake_info.borrows_woken += 1;
                        wake_info.exotics_woken += 1;
                        wake_info.final_wake = kind;
                        break 'unborrowed;
                    },
                    kind@BorrowKind::Ref => {
                        unsafe { self.stack.pop().unwrap_unchecked().wake() };
                        wake_info.borrows_woken += 1;
                        wake_info.final_wake = kind;
                        // copied from wake_from_inexotic
                        'inexotic: while let Some(last) = self.stack.last() {
                            match last.kind {
                                BorrowKind::Mut => break 'inexotic,
                                kind@BorrowKind::Exotic => {
                                    unsafe { self.stack.pop().unwrap_unchecked().wake() };
                                    wake_info.borrows_woken += 1;
                                    wake_info.exotics_woken += 1;
                                    wake_info.final_wake = kind;
                                    break 'inexotic;
                                },
                                kind@BorrowKind::Ref => {
                                    unsafe { self.stack.pop().unwrap_unchecked().wake() };
                                    wake_info.borrows_woken += 1;
                                    wake_info.final_wake = kind;
                                    continue 'inexotic;
                                },
                                BorrowKind::Skip => {
                                    unsafe { drop(self.stack.pop().unwrap_unchecked()) }
                                    continue 'inexotic;
                                },
                            }
                            #[allow(unreachable_code)]
                            {
                                unreachable!()
                            }
                        }
                        break 'unborrowed;
                    },
                    BorrowKind::Skip => {
                        unsafe { drop(self.stack.pop().unwrap_unchecked()) }
                        continue 'unborrowed;
                    },
                }
                #[allow(unreachable_code)]
                {
                    unreachable!()
                }
            }
            wake_info
        }
    }
}

use stack::{BorrowId, BorrowStack, WakeInfo};

mod states {
    use stack::BorrowKind;

    use super::*;

    pub trait PtrState: Sized {
        fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
            let _ = state;
            AdditionalWork::Nope
        }
    }
    pub unsafe trait RefState: PtrState {
        const BORROW_KIND: BorrowKind;

        fn un_borrow(self, state: &mut ContractState) -> AdditionalWork;
    }
    impl<T: RefState> PtrState for T {
        fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
            // TODO: handle mutex poisoning
            let mut state = state.lock().unwrap();
            let work = self.un_borrow(&mut*state);
            state.record_borrow_release(Self::BORROW_KIND);
            drop(state);
            work
        }
    }
    pub unsafe trait RefMutState: RefState {}

    pub struct Empty;
    impl PtrState for Empty {}

    pub struct Owner;
    unsafe impl RefState for Owner {
        const BORROW_KIND: BorrowKind = BorrowKind::Mut;

        fn un_borrow(self, state: &mut ContractState) -> AdditionalWork {
            AdditionalWork::Drop
        }
    }
    unsafe impl RefMutState for Owner {}
    impl Owner {
        pub unsafe fn init() -> Owner {
            Owner
        }
    }

    pub struct Weak(usize);
    impl PtrState for Weak {}
    impl Weak {
        pub fn try_upgrade(self, mut_number: usize) -> Result<Ref, Empty> {
            if self.0 == mut_number {
                Ok(Ref)
            } else {
                Err(Empty)
            }
        }
    }

    pub struct Ref;
    unsafe impl RefState for Ref {
        const BORROW_KIND: BorrowKind = BorrowKind::Ref;

        fn un_borrow(self, state: &mut ContractState) -> AdditionalWork {
            if state.borrow_count == 0 {
                state.wake_from_unborrowed()
            } else {
                AdditionalWork::Nope
            }
        }
    }
    impl Ref {
        pub fn downgrade(self, mut_number: usize) -> Weak {
            Weak(mut_number)
        }
    }

    pub struct UpgradableRef;
    unsafe impl RefState for UpgradableRef {
        const BORROW_KIND: BorrowKind = BorrowKind::Exotic;

        fn un_borrow(self, state: &mut ContractState) -> AdditionalWork {
            if state.exotic_count == 0 {
                state.wakers.flush_upgrade_buffer();
                if state.borrow_count == 0 {
                    state.wake_from_unborrowed()
                } else {
                    state.wake_from_inexotic();
                    AdditionalWork::Nope
                }
            } else {
                AdditionalWork::Nope
            }
        }
    }

    impl UpgradableRef {
        pub fn into_ref(self) -> Ref {
            Ref
        }

        pub unsafe fn assert_unique(self) -> RefMut {
            RefMut
        }
    }

    pub struct RefMut;
    unsafe impl RefState for RefMut {
        const BORROW_KIND: BorrowKind = BorrowKind::Exotic;

        fn un_borrow(self, state: &mut ContractState) -> AdditionalWork {
            state.wake_from_unborrowed()
        }
    }
    unsafe impl RefMutState for RefMut {}
    impl RefMut {
        pub fn into_ref(self) -> Ref {
            Ref
        }

        pub fn downgrade(self) -> UpgradableRef {
            UpgradableRef
        }
    }
}

struct Pointer<T, S: states::PtrState> {
    /// only valid to be uninit if about to forget
    inner: MaybeUninit<PointerInner<T, S>>
}

struct PointerInner<T, S: states::PtrState> {
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
}

struct PointerAwait<T, S: states::PtrState> {
    id: BorrowId,
    inner: Option<PointerInner<T, S>>
}

impl<T, S: states::PtrState> PointerAwait<T, S> {
    pub fn poll_val(&mut self, cx: &mut Context) -> Poll<Pointer<T, S>> {
        let pointer = self.inner.as_mut().unwrap();
        let mut state = pointer.contract().contract_state.lock().unwrap();
        match state.wakers.poll_at(&self.id, cx) {
            Poll::Ready(()) => unsafe {
                drop(state);
                let _ = pointer;
                Poll::Ready(Pointer::handle_drop(self.inner.take().unwrap_unchecked()))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, S: states::PtrState> Drop for PointerAwait<T, S> {
    fn drop(&mut self) {
        let Some(pointer) = self.inner.take() else {
            return
        };
        let mut state = pointer.contract().contract_state.lock().unwrap();
        let res = state.wakers.close(&self.id);
        drop(state);
        match res {
            // dropped before woken
            Ok(()) => drop(Pointer::handle_drop(pointer.map_state(|_| states::Empty))),
            // woken, must handle
            Err(()) => drop(Pointer::handle_drop(pointer)),
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

impl<T, S: states::PtrState> Drop for Pointer<T, S> {
    fn drop(&mut self) {
        let PointerInner { state, contract } = unsafe { std::mem::replace(&mut self.inner, MaybeUninit::uninit()).assume_init() };
        match state.un_track(&unsafe { contract.as_ref() }.contract_state) {
            AdditionalWork::Nope => (),
            AdditionalWork::Drop => unsafe { contract.as_ref().value.get().as_mut().unwrap_unchecked().assume_init_drop() },
        }
        if unsafe { contract.as_ref() }.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            drop(unsafe { Box::from_raw(contract.as_ptr()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vibe_check() {
        // ...
    }
}