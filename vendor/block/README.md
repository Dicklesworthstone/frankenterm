Rust interface for Apple's C language extension of blocks.

## FrankenTerm local package patch (ft-m5lhm)

Imported without omitted files from the crates.io `block` 0.1.6 archive.
Archive SHA-256: `0d8c1fef690941d3e7788d328517591fecc684c084084702d6ff1641e993699a`,
verified against the original workspace Cargo.lock registry checksum before import.
The archive contains Cargo.toml, README.md, src/lib.rs and src/test_utils.rs.
Its Cargo.toml declares author Steven Sheldon and license MIT; it contains no
standalone license file. These published metadata and all original files are retained.

Local changes: replace the uninhabited foreign class declaration with inhabited
`#[repr(C)]` storage containing `UnsafeCell<[*mut c_void; 32]>`, and acquire its
address using `ptr::addr_of!` instead of creating a reference. The array layout
comes from LLVM compiler-rt's Block_private.h declaration of
`_NSConcreteStackBlock[32]`; pinned block2 0.6.2 uses the same inhabited storage
size with interior-mutability marking. No runtime storage is read by Rust and
no block header, calling convention, flags, copy helper or dispose helper changes.

ABI references:
- https://clang.llvm.org/docs/Block-ABI-Apple.html
- https://github.com/llvm/llvm-project/blob/main/compiler-rt/lib/BlocksRuntime/Block_private.h

The package remains excluded from workspace membership and overrides the
registry dependency for Cocoa and cocoa-foundation. Native regression lives in
the existing window macOS module:
`os::macos::block_abi_regression::native_block_stack_heap_copy_invoke_and_final_dispose`.
It exercises actual libSystem copy/retain/release and invocation with an owned
capture whose final disposal is counted. Execute it on macOS; a Linux build
with the module excluded is not proof. Fresh native compile/future-incompatibility
evidence and that test's terminal result are required before claiming resolution.
The published crate's own historical tests reference an unbundled `test_utils`
development dependency. Its unusable path declaration is removed from this
local package so the source bundle is self-contained for RCH; this does not
fabricate the missing fixture or qualify those historical tests. The native
window regression above is the supported validation of this repair.

For more information on the specifics of the block implementation, see
Clang's documentation: http://clang.llvm.org/docs/Block-ABI-Apple.html

## Invoking blocks

The `Block` struct is used for invoking blocks from Objective-C. For example,
consider this Objective-C function:

``` objc
int32_t sum(int32_t (^block)(int32_t, int32_t)) {
    return block(5, 8);
}
```

We could write it in Rust as the following:

``` rust
unsafe fn sum(block: &Block<(i32, i32), i32>) -> i32 {
    block.call((5, 8))
}
```

Note the extra parentheses in the `call` method, since the arguments must be
passed as a tuple.

## Creating blocks

Creating a block to pass to Objective-C can be done with the `ConcreteBlock`
struct. For example, to create a block that adds two `i32`s, we could write:

``` rust
let block = ConcreteBlock::new(|a: i32, b: i32| a + b);
let block = block.copy();
assert!(unsafe { block.call((5, 8)) } == 13);
```

It is important to copy your block to the heap (with the `copy` method) before
passing it to Objective-C; this is because our `ConcreteBlock` is only meant
to be copied once, and we can enforce this in Rust, but if Objective-C code
were to copy it twice we could have a double free.
