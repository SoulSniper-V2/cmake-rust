execute_process(
  COMMAND printf [=[caf\351\n]=]
  OUTPUT_VARIABLE out
  ERROR_VARIABLE err
  RESULT_VARIABLE result
  ECHO_OUTPUT_VARIABLE
)
file(WRITE process-out.bin "${out}")
file(READ process-out.bin hex HEX)
string(LENGTH "${err}" errlen)
message("result=${result} hex=${hex} errlen=${errlen}")
