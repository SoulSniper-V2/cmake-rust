file(WRITE input "a;b;c\n")
file(SHA1 input out)
message("[${out}]")
