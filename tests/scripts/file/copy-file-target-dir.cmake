file(WRITE a x)
file(MAKE_DIRECTORY b)
file(COPY_FILE a b RESULT out)
message("[${out}]")
