#+build !linux
package micromeasure

@(private)
Counter_Platform :: struct {}

counters_open :: proc() -> Counter_Set {return {}}
counters_close :: proc(set: ^Counter_Set) {set^ = {}}
counters_begin :: proc(set: ^Counter_Set) {}
counters_end :: proc(set: ^Counter_Set) {}
