file(WRITE input "a;b;c\n")
file(SHA384 input out)
message("[${out}]")
