// Anti-optimization helper.
package micromeasure

import "base:intrinsics"

// black_box pushes a value through a volatile cell. Apply it to inputs before
// computation and to the final result. Output-only use does not prevent
// constant folding or loop-invariant computation. A pointer value does not
// make all pointed-to memory opaque. Inspect generated code for tiny bodies.
black_box :: proc(value: $T) -> T {
	local := value
	intrinsics.volatile_store(&local, value)
	return intrinsics.volatile_load(&local)
}
