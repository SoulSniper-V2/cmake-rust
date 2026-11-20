execute_process(
  COMMAND /bin/sh -c "sleep 0.1; printf finished > completion"
  TIMEOUT 0.025 RESULT_VARIABLE result
)
execute_process(
  COMMAND /bin/sh -c "for delay in 1 2 3 4 5 6 7 8 9 10; do test -f completion && break; sleep 0.05; done; cat completion"
  OUTPUT_VARIABLE completion
)
message("${result}|${completion}")
