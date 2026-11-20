file(LOCK locks/one/../file TIMEOUT 0)
file(LOCK locks//file TIMEOUT 0 RESULT_VARIABLE result)
message("[${result}]")
