use crate::{Ref, RefMut};


impl<T> Ref<T, ()> {
    pub fn destructure(self) -> () {
        self.context(|this, ctx| {
            let _ = this;
            drop(ctx)
        })
    }
}

impl<T, B0> Ref<T, (B0,)> {
    pub fn destructure(self) -> (Ref<T, B0>,) {
        self.context(|(b0,), ctx| {
            (
                ctx.lift_ref(b0),
            )
        })
    }
}

impl<T, B0, B1> Ref<T, (B0, B1)> {
    pub fn destructure(self) -> (Ref<T, B0>, Ref<T, B1>) {
        self.context(|(b0, b1), ctx| {
            (
                ctx.lift_ref(b0),
                ctx.lift_ref(b1)
            )
        })
    }
}

impl<T, B0, B1, B2> Ref<T, (B0, B1, B2)> {
    pub fn destructure(self) -> (Ref<T, B0>, Ref<T, B1>, Ref<T, B2>) {
        self.context(|(b0, b1, b2), ctx| {
            (
                ctx.lift_ref(b0),
                ctx.lift_ref(b1),
                ctx.lift_ref(b2)
            )
        })
    }
}

impl<T, B0, B1, B2, B3> Ref<T, (B0, B1, B2, B3)> {
    pub fn destructure(self) -> (Ref<T, B0>, Ref<T, B1>, Ref<T, B2>, Ref<T, B3>) {
        self.context(|(b0, b1, b2, b3), ctx| {
            (
                ctx.lift_ref(b0),
                ctx.lift_ref(b1),
                ctx.lift_ref(b2),
                ctx.lift_ref(b3)
            )
        })
    }
}

impl<T> RefMut<T, ()> {
    pub fn destructure(self) -> () {
        self.context(|this, ctx| {
            let _ = this;
            drop(ctx)
        })
    }
}

impl<T, B0> RefMut<T, (B0,)> {
    pub fn destructure(self) -> (RefMut<T, B0>,) {
        self.context(|(b0,), ctx| {
            (
                ctx.lift_mut(b0),
            )
        })
    }
}

impl<T, B0, B1> RefMut<T, (B0, B1)> {
    pub fn destructure(self) -> (RefMut<T, B0>, RefMut<T, B1>) {
        self.context(|(b0, b1), ctx| {
            (
                ctx.lift_mut(b0),
                ctx.lift_mut(b1)
            )
        })
    }
}

impl<T, B0, B1, B2> RefMut<T, (B0, B1, B2)> {
    pub fn destructure(self) -> (RefMut<T, B0>, RefMut<T, B1>, RefMut<T, B2>) {
        self.context(|(b0, b1, b2), ctx| {
            (
                ctx.lift_mut(b0),
                ctx.lift_mut(b1),
                ctx.lift_mut(b2)
            )
        })
    }
}

impl<T, B0, B1, B2, B3> RefMut<T, (B0, B1, B2, B3)> {
    pub fn destructure(self) -> (RefMut<T, B0>, RefMut<T, B1>, RefMut<T, B2>, RefMut<T, B3>) {
        self.context(|(b0, b1, b2, b3), ctx| {
            (
                ctx.lift_mut(b0),
                ctx.lift_mut(b1),
                ctx.lift_mut(b2),
                ctx.lift_mut(b3)
            )
        })
    }
}