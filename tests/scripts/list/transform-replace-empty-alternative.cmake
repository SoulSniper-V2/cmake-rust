cmake_minimum_required(VERSION 4.1)
set(items ab ba)
list(TRANSFORM items REPLACE "(|a)" X)
message("[${items}]")

cmake_policy(SET CMP0186 OLD)
set(items baa)
list(TRANSFORM items REPLACE "(^|a)" X)
message("[${items}]")
