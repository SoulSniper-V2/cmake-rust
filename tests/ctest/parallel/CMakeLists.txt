cmake_minimum_required(VERSION 4.1)
project(CTestParallel NONE)
enable_testing()

add_test(NAME first COMMAND /bin/sh -c
  "touch first; until test -f second; do sleep 0.01; done; sleep 0.2; touch completed")
set_tests_properties(first PROPERTIES RESOURCE_LOCK locked TIMEOUT 5)
add_test(NAME competing COMMAND /bin/sh -c "test -f completed; exit $?")
set_tests_properties(competing PROPERTIES RESOURCE_LOCK locked)
add_test(NAME second COMMAND /bin/sh -c
  "until test -f first; do sleep 0.01; done; touch second")
set_tests_properties(second PROPERTIES TIMEOUT 5)
add_test(NAME serial COMMAND /bin/sh -c "test -f completed && test -f second")
set_tests_properties(serial PROPERTIES RUN_SERIAL ON)
