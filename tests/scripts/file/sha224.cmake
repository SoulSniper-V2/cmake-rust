file(WRITE input "a;b;c\n")
file(SHA224 input out)
message("[${out}]")
