execute_process(COMMAND /bin/sh -c "mkdir work; printf data > work/input")
execute_process(
  COMMAND /bin/cat WORKING_DIRECTORY work INPUT_FILE input OUTPUT_FILE output
  RESULT_VARIABLE result
)
execute_process(COMMAND /bin/cat work/output OUTPUT_VARIABLE output)
message("${result}|${output}")
