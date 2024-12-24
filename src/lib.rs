use std::{ops::{Deref, DerefMut}, pin::Pin};

pub(crate) mod contract;
pub mod scope;
pub mod collections;

// API WORK
// TODO: Type parameter varience checks
// TODO: Miri safety checks

pub struct Weak<T> {
    inner: contract::Pointer<T, contract::states::Weak>
}

impl<T> Weak<T> {
    fn from_raw(inner: contract::Pointer<T, contract::states::Weak>) -> Weak<T> {
        Weak {
            inner
        }
    }

    fn into_raw(self) -> contract::Pointer<T, contract::states::Weak> {
        self.inner
    }

    pub fn upgrade(self) -> Option<Ref<T>> {
        self.into_raw().upgrade().map(Ref::from_raw)
    }
}

pub struct Ref<T> {
    inner: contract::Pointer<T, contract::states::Ref>
}

impl<T> Deref for Ref<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.get_ref()
    }
}

impl<T> Ref<T> {
    fn from_raw(inner: contract::Pointer<T, contract::states::Ref>) -> Ref<T> {
        Ref {
            inner
        }
    }

    fn into_raw(self) -> contract::Pointer<T, contract::states::Ref> {
        self.inner
    }

    pub fn unpin(ptr: Pin<Ref<T>>) -> Ref<T> {
        unsafe { Pin::into_inner_unchecked(ptr) }
    }
}

pub struct UpgRef<T> {
    inner: contract::Pointer<T, contract::states::UpgradableRef>
}

impl<T> Deref for UpgRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.get_ref()
    }
}

impl<T> UpgRef<T> {
    fn from_raw(inner: contract::Pointer<T, contract::states::UpgradableRef>) -> UpgRef<T> {
        UpgRef {
            inner
        }
    }

    fn into_raw(self) -> contract::Pointer<T, contract::states::UpgradableRef> {
        self.inner
    }

    pub fn as_ref(&self) -> Ref<T> {
        Ref::from_raw(self.inner.as_ref())
    }

    pub fn pinned_as_ref(ptr: &Pin<UpgRef<T>>) -> Ref<T> {
        unsafe { (&*(ptr as *const Pin<UpgRef<T>> as *const UpgRef<T>)).as_ref() }
    }

    pub async fn upgrade(self) -> Mut<T> {
        // TODO: make async type explicit for more control
        todo!()
    }

    pub async fn pinned_upgrade(ptr: Pin<UpgRef<T>>) -> Pin<Mut<T>> {
        // TODO: make async type explicit for more control
        todo!()
    }
}

pub struct Mut<T> {
    inner: contract::Pointer<T, contract::states::RefMut>
}

impl<T> Deref for Mut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.get_ref()
    }
}

impl<T> DerefMut for Mut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.get_mut()
    }
}

impl<T> Mut<T> {
    fn from_raw(inner: contract::Pointer<T, contract::states::RefMut>) -> Mut<T> {
        Mut {
            inner
        }
    }

    fn into_raw(self) -> contract::Pointer<T, contract::states::RefMut> {
        self.inner
    }

    pub fn into_ref(self) -> Ref<T> {
        Ref::from_raw(self.into_raw().into_ref())
    }

    pub fn pinned_into_ref(ptr: Pin<Mut<T>>) -> Ref<T> {
        unsafe { Pin::into_inner_unchecked(ptr).into_ref() }
    }

    pub fn downgrade(self) -> UpgRef<T> {
        UpgRef::from_raw(self.into_raw().downgrade())
    }

    pub fn pinned_downgrade(ptr: Pin<Mut<T>>) -> Pin<UpgRef<T>> {
        unsafe { Pin::new_unchecked(Pin::into_inner_unchecked(ptr).downgrade()) }
    }
}