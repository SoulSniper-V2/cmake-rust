set(VAR value)
file(CONFIGURE OUTPUT configured CONTENT [=[#cmakedefine01 VAR
@VAR@-${VAR}]=] @ONLY)
file(READ configured out HEX)
message("[${out}]")
