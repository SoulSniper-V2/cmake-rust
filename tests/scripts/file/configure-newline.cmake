file(CONFIGURE OUTPUT configured CONTENT "one\ntwo" NEWLINE_STYLE DOS)
file(READ configured out HEX)
message("[${out}]")
