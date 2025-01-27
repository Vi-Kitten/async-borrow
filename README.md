# README

Borrowing without lifetimes using async.

*early stages, not documented or sufficiently tested*

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