file(LOCK a/b DIRECTORY TIMEOUT 0)
file(LOCK a/b/cmake.lock RESULT_VARIABLE out TIMEOUT 0)
message("[${out}]")
