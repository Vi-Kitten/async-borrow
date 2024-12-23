use std::pin::Pin;

type Unit = ();

enum Void {}

impl From<Void> for Unit {
    fn from(value: Void) -> Self {
        match value {}
    }
}

// Access

trait AccessModifier {
    type Modify<T>;
}

pub struct Moveable;

pub struct Pinned;

impl AccessModifier for Moveable {
    type Modify<T> = T;
}

impl AccessModifier for Pinned {
    type Modify<T> = Pin<T>;
}

// Sharing

trait SharePermissions {
    type LeaseRef;
    type LeaseMut: Into<Self::LeaseRef>;
}

pub struct Isolated;

pub struct Immutable;

pub struct Mutable;

impl SharePermissions for Isolated {
    type LeaseRef = Void;
    type LeaseMut = Void;
}

impl SharePermissions for Immutable {
    type LeaseRef = Unit;
    type LeaseMut = Void;
}

impl SharePermissions for Mutable {
    type LeaseRef = Unit;
    type LeaseMut = Unit;
}