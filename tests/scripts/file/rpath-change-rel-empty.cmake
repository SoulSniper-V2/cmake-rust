file(COPY_FILE data/rel prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/a" NEW_RPATH "")
message(STATUS "done")
