file(READ_ELF data/prog RPATH rp RUNPATH ru CAPTURE_ERROR err)
message(STATUS "rp=[${rp}] ru=[${ru}] err=[${err}]")
file(READ_ELF data/both RPATH rp2 RUNPATH ru2)
message(STATUS "rp2=[${rp2}] ru2=[${ru2}]")
