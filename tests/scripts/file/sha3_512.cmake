file(WRITE input "a;b;c\n")
file(SHA3_512 input out)
message("[${out}]")
