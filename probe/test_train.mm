// test_train.mm — standalone fake "training loop" used to validate jade_probe.
// Deliberately contains ZERO jade instrumentation: no __JADE_ prints, no
// probe headers. Everything the probe reports must come from interposition.
//
// It creates a PRIVATE (VRAM-only) weight matrix, a shared gradient buffer,
// and runs an SGD-style compute kernel for a number of steps.

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

static NSString *const kKernelSrc = @R"(
#include <metal_stdlib>
using namespace metal;
kernel void sgd_step(device float *W [[buffer(0)]],
                     device const float *G [[buffer(1)]],
                     constant float &lr [[buffer(2)]],
                     uint i [[thread_position_in_grid]]) {
  W[i] -= lr * G[i];
}
)";

int main(void) {
  @autoreleasepool {
    const NSUInteger dim = 256;                       // 256x256 weight matrix
    const NSUInteger n = dim * dim;
    const NSUInteger bytes = n * sizeof(float);

    id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
    NSError *err = nil;
    id<MTLLibrary> lib = [dev newLibraryWithSource:kKernelSrc options:nil error:&err];
    if (!lib) { NSLog(@"compile failed: %@", err); return 1; }
    id<MTLComputePipelineState> pso =
        [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"sgd_step"]
                                           error:&err];
    id<MTLCommandQueue> queue = [dev newCommandQueue];

    // Weights: PRIVATE storage — lives in VRAM, never mapped to CPU.
    id<MTLBuffer> weights = [dev newBufferWithLength:bytes
                                             options:MTLResourceStorageModePrivate];
    weights.label = @"model.weights";

    // Gradients: shared, refreshed from CPU each step.
    id<MTLBuffer> grads = [dev newBufferWithLength:bytes
                                           options:MTLResourceStorageModeShared];
    grads.label = @"model.grads";

    // Initialize weights via blit from a temporary shared buffer.
    id<MTLBuffer> init = [dev newBufferWithLength:bytes
                                          options:MTLResourceStorageModeShared];
    float *ip = (float *)init.contents;
    srandom(42);
    for (NSUInteger i = 0; i < n; i++)
      ip[i] = ((float)random() / RAND_MAX - 0.5f) * 0.2f;
    {
      id<MTLCommandBuffer> cb = [queue commandBuffer];
      cb.label = @"init.weights";
      id<MTLBlitCommandEncoder> blit = [cb blitCommandEncoder];
      [blit copyFromBuffer:init sourceOffset:0 toBuffer:weights destinationOffset:0 size:bytes];
      [blit endEncoding];
      [cb commit];
      [cb waitUntilCompleted];
    }

    float lr = 0.05f;
    for (int step = 0; step < 40; step++) {
      // Fake gradients on CPU.
      float *gp = (float *)grads.contents;
      for (NSUInteger i = 0; i < n; i++)
        gp[i] = ((float)random() / RAND_MAX - 0.5f) * 0.02f;

      id<MTLCommandBuffer> cb = [queue commandBuffer];
      cb.label = @"train_step";
      id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
      [enc setComputePipelineState:pso];
      [enc setBuffer:weights offset:0 atIndex:0];
      [enc setBuffer:grads offset:0 atIndex:1];
      [enc setBytes:&lr length:sizeof(lr) atIndex:2];
      [enc dispatchThreads:MTLSizeMake(n, 1, 1)
          threadsPerThreadgroup:MTLSizeMake(256, 1, 1)];
      [enc endEncoding];
      [cb commit];
      [cb waitUntilCompleted];
      usleep(20000);  // simulate other per-step work
    }
    printf("training loop done\n");
  }
  return 0;
}
