file(COPY_FILE data/prog prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/first/lib" NEW_RPATH "/this/path/is/far/too/long/to/fit")
