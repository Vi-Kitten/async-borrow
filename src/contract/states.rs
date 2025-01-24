use borrow_queue::BorrowKind;

use super::*;

pub trait PtrState: Sized {
    #[must_use]
    fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
        let _ = state;
        AdditionalWork::Noop
    }
}
pub unsafe trait RefState: PtrState {
    const BORROW_KIND: BorrowKind;

    fn release(self, _state: &mut ContractState) {}
}
impl<T: RefState> PtrState for T {
    fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
        // TODO: handle mutex poisoning
        let mut state = state.lock().unwrap();
        self.release(&mut*state);
        state.reduce_borrow(&Self::BORROW_KIND)
    }
}
pub unsafe trait RefMutState: RefState {}

#[derive(Clone, Copy)]
pub struct Borrowless;
impl PtrState for Borrowless {}

pub struct Owner;
impl PtrState for Owner {
    fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
        self.shared().un_track(state)
    }
}

impl Owner {
    pub unsafe fn init() -> Self {
        Owner
    }

    pub fn shared(self) -> Host {
        Host
    }

    pub fn share_ref(&mut self) -> Ref {
        Ref
    }

    pub fn share_mut(&mut self) -> RefMut {
        RefMut
    }
}

pub struct Host;
impl PtrState for Host {
    fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
        // TODO: handle mutex poisoning
        let mut state = state.lock().unwrap();
        match state.cancel_reaquire() {
            Some(()) => AdditionalWork::Noop,
            None => AdditionalWork::Drop,
        }
    }
}

impl Host {
    pub unsafe fn assert_unshared(self) -> Owner {
        Owner
    }
}

// u64 to ensure it can count is high enough
#[derive(Clone)]
pub struct Weak(pub u64);
impl PtrState for Weak {}
impl Weak {
    pub fn try_upgrade(self, mut_number: u64) -> Result<Ref, Borrowless> {
        if self.0 == mut_number {
            Ok(Ref)
        } else {
            Err(Borrowless)
        }
    }
}

#[derive(Clone)]
pub struct Ref;
unsafe impl RefState for Ref {
    const BORROW_KIND: BorrowKind = BorrowKind::Ref;
}

impl Ref {
    pub fn downgrade(self, mut_number: u64) -> Weak {
        Weak(mut_number)
    }
}

#[derive(Clone)]
pub struct UpgradableRef;
unsafe impl RefState for UpgradableRef {
    const BORROW_KIND: BorrowKind = BorrowKind::Upgradable;

    fn release(self, state: &mut ContractState) {
        if state.counts.exotic_count == 0 {
            state.wakers.end_upgrading();
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
    const BORROW_KIND: BorrowKind = BorrowKind::Upgradable;
}
unsafe impl RefMutState for RefMut {}
impl RefMut {
    pub fn into_ref(self) -> Ref {
        Ref
    }

    pub fn downgrade(self) -> UpgradableRef {
        UpgradableRef
    }

    pub unsafe fn use_as_forward(&mut self) -> RefMut {
        RefMut
    }
}