# Verbatim from Kitware CMake 4.1.0 Tests/RunCMake/string/TimestampEmpty.cmake
# (BSD-3-Clause, Kitware Inc.) - differential coverage of upstream test content.
set(ENV{SOURCE_DATE_EPOCH} "")
string(TIMESTAMP RESULT "%Y-%m-%d %H:%M:%S" UTC)
message("RESULT=${RESULT}")
