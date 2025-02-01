# README

Borrowing without lifetimes using async.

*This crate is currently in early stages, and is marked as experimental.*
*It is insufficiently documented, tested, and lacks safety garuntees on certain seemingly safe parts of the API.*
*I have tried my best to be as explicit in documentation about this as I can be, but I provide no garuntees.*

> **As of right now this crate should only be used for experimentation, if you are using this in safety critical software, you have been warned.**

Async borrow solves the issue of async scopes from the opposite angle, by recreating borrowing and ownership semantics in a purely heap based and async compatable environment. Borrowing is achieved by the use of by-value methods that return smart pointers and a future to re-acquire previous privillages once the reference count reaches 0.

Heres a thing that this funny little library can do:

```rs
ShareBox::new((0_i8, 0_i8))
    .spawn_mut(|rf_mut| tokio::spawn(async move {
        rf_mut
            .cleave_mut(|rf_mut| {
                let (mut rf_mut_left, mut rf_mut_right) = rf_mut.scope(|(a, b), context| {
                    (context.contextualise_mut(a), context.contextualise_mut(b))
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
```

## Yanked versions

- `v0.1.0` was yanked due to safety violations from implementing dereferencing on certain futures. This prompted a majour version increase to `v0.2.0` and the inclusion of the `experimental = true` badge.