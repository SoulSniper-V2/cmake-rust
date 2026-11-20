file(COPY_FILE data/nodyn prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/a" NEW_RPATH "/b")
message(STATUS "done")
