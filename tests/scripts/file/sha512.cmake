file(WRITE input "a;b;c\n")
file(SHA512 input out)
message("[${out}]")
