file(COPY_FILE data/plain prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/a" NEW_RPATH "")
message(STATUS "done")
