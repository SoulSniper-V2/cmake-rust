# Verbatim from Kitware CMake 4.1.0 Tests/RunCMake/string/TimestampInvalid.cmake
# (BSD-3-Clause, Kitware Inc.) - differential coverage of upstream test content.
set(ENV{SOURCE_DATE_EPOCH} "invalid-integer")
string(TIMESTAMP RESULT "%Y-%m-%d %H:%M:%S" UTC)
message("RESULT=${RESULT}")
