file(WRITE a x)
file(COPY_FILE a missing/b RESULT out)
message("[${out}]")
