use std::{ops::{Deref, DerefMut}, pin::Pin, future::Future};

use crate::{Mut, Ref};

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

pub struct Isolated;

pub struct Immutable;

pub struct Mutable;

// Collection behaviour

trait UnsharedEntry {
    type Modify<T>;
    type Elem;
    fn as_ref(&self) -> &Self::Elem;
    fn as_mut(&mut self) -> Self::Modify<&mut Self::Elem>;
    fn finalize(self) -> Pin<Mut<Self::Elem>>;
}

trait SharableEntry: UnsharedEntry {
    type Shared: SharedEntry<Elem = Self::Elem, Unshared = Self>;
    fn share_ref(self) -> (Self::Shared, Ref<Self::Elem>);
}

trait MutSharableEntry: SharableEntry {
    fn share_mut(self) -> (Self::Shared, Self::Modify<Mut<Self::Elem>>);
}

trait SharedEntry: Future<Output = Self::Unshared> {
    type Elem;
    type Unshared: SharableEntry<Elem = Self::Elem, Shared = Self>;
}

trait ImmutablySharedEntry: SharableEntry {
    fn peek(&self) -> &Self::Elem;
    fn visit(&self) -> Ref<Self::Elem>;
}