//! A macro for defining #[cfg] if-else statements.
//!
//! The macro provided by this crate, `cfg_if`, is similar to the `if/elif` C
//! preprocessor macro by allowing definition of a cascade of `#[cfg]`
//! cases, emitting the implementation which matches first.
//!
//! This allows you to conveniently provide a long list `#[cfg]`'d blocks of code
//! without having to rewrite each clause multiple times.
//!
//! # Example
//!
//! ```
//! cfg_if::cfg_if! {
//!     if #[cfg(unix)] {
//!         fn foo() { /* unix specific functionality */ }
//!     } else if #[cfg(target_pointer_width = "32")] {
//!         fn foo() { /* non-unix, 32-bit functionality */ }
//!     } else {
//!         fn foo() { /* fallback implementation */ }
//!     }
//! }
//!
//! # fn main() {}
//! ```
//!
//! # Supported item types
//!
//! The macro requires the emitted blocks to all expand to item definitions.

#![no_std]
#![doc(html_root_url = "https://docs.rs/cfg-if")]
#![cfg_attr(test, allow(unexpected_cfgs))]

/// The main macro provided by this crate. See crate level documentation for
/// more information.
#[macro_export]
macro_rules! cfg_if {
    ($(
        if #[cfg($meta:meta)] { $($tokens:tt)* }
    ) else * else {
        $($tokens2:tt)*
    }) => {
        $crate::cfg_if! {
            @__items
            () ;
            $( ( ($meta) ($($tokens)*) ), )*
            ( () ($($tokens2)*) ),
        }
    };

    (
        if #[cfg($i_met:meta)] { $($i_tokens:tt)* }
        $(
            else if #[cfg($e_met:meta)] { $($e_tokens:tt)* }
        )*
    ) => {
        $crate::cfg_if! {
            @__items
            () ;
            ( ($i_met) ($($i_tokens)*) ),
            $( ( ($e_met) ($($e_tokens)*) ), )*
            ( () () ),
        }
    };

    (@__items ($($not:meta,)*) ; ) => {};
    (@__items ($($not:meta,)*) ; ( ($($m:meta),*) ($($tokens:tt)*) ), $($rest:tt)*) => {
        #[cfg(all($($m,)* not(any($($not),*))))] $crate::cfg_if! { @__identity $($tokens)* }
        $crate::cfg_if! { @__items ($($not,)* $($m,)*) ; $($rest)* }
    };

    (@__identity $( $tokens:tt )* ) => {
        $( $tokens )*
    };
}

#[cfg(test)]
mod tests;
