file(COPY_FILE data/prog prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/first/lib" NEW_RPATH "/n"
  INSTALL_REMOVE_ENVIRONMENT_RPATH)
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
