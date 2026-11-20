file(WRITE input "a;b;c\n")
file(MD5 input out)
message("[${out}]")
