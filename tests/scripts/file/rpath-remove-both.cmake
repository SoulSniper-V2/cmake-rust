file(COPY_FILE data/both prog)
file(RPATH_REMOVE FILE prog)
file(READ_ELF prog RPATH rp RUNPATH ru)
message(STATUS "rp=[${rp}] ru=[${ru}]")
