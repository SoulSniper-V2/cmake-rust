file(WRITE input "a;b;c\n")
file(SHA256 input out)
message("[${out}]")
