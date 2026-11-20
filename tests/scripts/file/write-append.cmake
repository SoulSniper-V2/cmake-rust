file(WRITE one/two/file hello " " world)
file(APPEND one/two/file "!" "\n" after)
file(READ one/two/file out HEX)
message("[${out}]")
