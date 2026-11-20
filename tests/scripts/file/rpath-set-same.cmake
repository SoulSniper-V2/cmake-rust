file(COPY_FILE data/prog prog)
file(RPATH_SET FILE prog NEW_RPATH "/first/lib:/second/lib")
message(STATUS "done")
