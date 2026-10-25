# Patched `block` 0.1.6

This is the source of MIT-licensed `block` 0.1.6, retained because
`gpui-kit` 0.8 reaches it through its macOS platform dependencies.

The functional source change replaces the uninhabited `enum Class {}`
used for `_NSConcreteStackBlock` with an inhabited zero-sized opaque C type.
That preserves the pointer ABI and removes Rust's `uninhabited_static`
future-incompatibility warning. The formerly implicit C ABI is also written
explicitly to keep current compilers warning-free. Remove this patch when the
GPUI dependency tree no longer uses `block` 0.1.x.
