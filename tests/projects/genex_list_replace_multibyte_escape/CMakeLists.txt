cmake_minimum_required(VERSION 3.20)
project(glr NONE)
# $<LIST:TRANSFORM,...,REPLACE> names the single byte after the backslash.
file(GENERATE OUTPUT out.txt CONTENT "$<LIST:TRANSFORM,a;b,REPLACE,a,x\\éy>")
