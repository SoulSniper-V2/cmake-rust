file(WRITE input "a;b;c\n")
file(SHA3_256 input out)
message("[${out}]")
