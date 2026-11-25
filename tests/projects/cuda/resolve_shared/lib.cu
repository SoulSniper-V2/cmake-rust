__global__ void kernel() {}
extern "C" void launch() { kernel<<<1,1>>>(); }
