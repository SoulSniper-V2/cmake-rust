# Verbatim from Kitware CMake 4.1.0 Tests/RunCMake/string/Timestamp.cmake
# (BSD-3-Clause, Kitware Inc.) - differential coverage of upstream test content.
set(ENV{SOURCE_DATE_EPOCH} "1123456789")
string(TIMESTAMP RESULT "%Y-%m-%d %H:%M:%S.%f %A=%a %B=%b %y day=%j wd=%w week=%U w_iso=%V %%I=%I epoch=%s TZ=%Z tz=%z" UTC)
message("RESULT=${RESULT}")
