file(WRITE input "a;b;c\n")
file(SHA3_224 input out)
message("[${out}]")
