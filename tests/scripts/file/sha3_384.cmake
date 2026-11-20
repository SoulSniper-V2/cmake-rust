file(WRITE input "a;b;c\n")
file(SHA3_384 input out)
message("[${out}]")
