/* Opaque libavcodec bridge: Rust never depends on FFmpeg struct layouts. */
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <pthread.h>
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libswscale/swscale.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include "convert_ptx.h"

// Keep one primary CUDA context alive for the process. A resize must not destroy
// a context while NVIDIA's asynchronous driver workers are still retiring work.
static AVBufferRef *shared_cuda_device;
static pthread_mutex_t device_mutex=PTHREAD_MUTEX_INITIALIZER;
static int acquire_cuda_device(AVBufferRef **device) {
    int result=0;
    pthread_mutex_lock(&device_mutex);
    if(!shared_cuda_device)
        result=av_hwdevice_ctx_create(&shared_cuda_device,AV_HWDEVICE_TYPE_CUDA,NULL,NULL,AV_CUDA_USE_PRIMARY_CONTEXT);
    if(result>=0) {
        *device=av_buffer_ref(shared_cuda_device);
        if(!*device) result=AVERROR(ENOMEM);
    }
    pthread_mutex_unlock(&device_mutex);
    return result;
}
typedef struct {
    AVCodecContext *codec;
    AVFrame *frame;
    AVPacket *packet;
    struct SwsContext *scale;
    int64_t next_pts;
    int source_height, source_width, rgba, gpu;
    AVBufferRef *device, *frames;
    CUcontext cuda;
    CUmodule module;
    CUfunction kernel;
    CUdeviceptr source;

} HyprNvenc;

void hypr_nvenc_free(HyprNvenc *enc) {
    if (!enc) return;
    avcodec_free_context(&enc->codec);
    if(enc->cuda) {
        CUcontext previous;
        if(cuCtxPushCurrent(enc->cuda)==CUDA_SUCCESS) {
            if(enc->source) cuMemFree(enc->source);
            if(enc->module) cuModuleUnload(enc->module);
            cuCtxPopCurrent(&previous);
        }
    }
    sws_freeContext(enc->scale);
    av_packet_free(&enc->packet);
    av_frame_free(&enc->frame);
    av_buffer_unref(&enc->frames);
    av_buffer_unref(&enc->device);
    free(enc);
}

static void describe_error(char *error, size_t size, const char *operation, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, detail, sizeof(detail));
    snprintf(error, size, "%s: %s", operation, detail);
}

HyprNvenc *hypr_nvenc_new(int source_width, int source_height, int rgba, int width, int height, int64_t bitrate, int fps,
                         int quality, int cqp, int gpu, int main_profile, char *error, size_t error_size) {
    const AVCodec *codec = avcodec_find_encoder_by_name("h264_nvenc");
    if (!codec) {
        snprintf(error, error_size, "FFmpeg was built without h264_nvenc");
        return NULL;
    }
    HyprNvenc *enc = calloc(1, sizeof(*enc));
    if (!enc) {
        describe_error(error, error_size, "allocate encoder", AVERROR(ENOMEM));
        return NULL;
    }
    int result = AVERROR(ENOMEM);
    const char *operation = "allocate FFmpeg objects";
    enc->source_height = source_height;
    enc->source_width = source_width; enc->rgba=rgba; enc->gpu=gpu;
    enc->codec = avcodec_alloc_context3(codec);
    enc->frame = av_frame_alloc();
    enc->packet = av_packet_alloc();
    if (!enc->codec || !enc->frame || !enc->packet) goto fail;
    AVCodecContext *ctx = enc->codec;
    ctx->width = width;
    ctx->height = height;
    ctx->pix_fmt = gpu ? AV_PIX_FMT_CUDA : AV_PIX_FMT_NV12;
    ctx->time_base = (AVRational){1, fps};
    ctx->framerate = (AVRational){fps, 1};
    ctx->gop_size = fps * 2;
    ctx->max_b_frames = 0;
    ctx->bit_rate = 0;
    ctx->rc_max_rate = bitrate;
    ctx->rc_buffer_size = 0;
    ctx->color_range = AVCOL_RANGE_JPEG;
    ctx->colorspace = AVCOL_SPC_BT709;
    ctx->color_primaries = AVCOL_PRI_BT709;
    ctx->color_trc = AVCOL_TRC_BT709;

#define SET_OPTION(name, value) do { \
    operation = "set NVENC option " name; \
    result = av_opt_set(ctx->priv_data, name, value, 0); \
    if (result < 0) goto fail; \
} while (0)
    SET_OPTION("preset", "p4");
    SET_OPTION("tune", "ull");
    SET_OPTION("zerolatency", "1");
    SET_OPTION("delay", "0");
    SET_OPTION("rc-lookahead", "0");
    SET_OPTION("forced-idr", "1");
    SET_OPTION("profile", main_profile ? "main" : "baseline");
    SET_OPTION("rc", cqp ? "constqp" : "vbr");
    // CQ intentionally prioritizes detail. Keep only a max-rate ceiling, not a two-frame VBV.
    operation="set quality target";
    result=av_opt_set_int(ctx->priv_data,cqp ? "qp" : "cq",quality,0);
    if(result<0)goto fail;
#undef SET_OPTION
    if(gpu) {
        operation="create CUDA device";
        result=acquire_cuda_device(&enc->device);
        if(result<0) goto fail;
        AVCUDADeviceContext *cuda=((AVHWDeviceContext*)enc->device->data)->hwctx;
        enc->cuda=cuda->cuda_ctx;
        enc->frames=av_hwframe_ctx_alloc(enc->device);
        if(!enc->frames){result=AVERROR(ENOMEM);goto fail;}
        AVHWFramesContext *frames=(AVHWFramesContext*)enc->frames->data;
        frames->format=AV_PIX_FMT_CUDA;frames->sw_format=AV_PIX_FMT_NV12;
        frames->width=width;frames->height=height;frames->initial_pool_size=2;
        operation="allocate CUDA frame pool";
        result=av_hwframe_ctx_init(enc->frames);if(result<0)goto fail;
        ctx->hw_frames_ctx=av_buffer_ref(enc->frames);
        if(!ctx->hw_frames_ctx){result=AVERROR(ENOMEM);goto fail;}
        CUcontext previous;
        operation="load CUDA conversion kernel";
        if(cuCtxPushCurrent(enc->cuda)!=CUDA_SUCCESS){result=AVERROR_EXTERNAL;goto fail;}
        CUresult cr=cuModuleLoadData(&enc->module,convert_ptx);
        if(cr==CUDA_SUCCESS) cr=cuModuleGetFunction(&enc->kernel,enc->module,"rgb_nv12");
        if(cr==CUDA_SUCCESS) cr=cuMemAlloc(&enc->source,(size_t)source_width*source_height*4);
        cuCtxPopCurrent(&previous);
        if(cr!=CUDA_SUCCESS){result=AVERROR_EXTERNAL;goto fail;}
    }
    operation = "open h264_nvenc (check NVIDIA driver and GPU)";
    result = avcodec_open2(ctx, codec, NULL);
    if (result < 0) goto fail;
    enc->frame->format = ctx->pix_fmt;
    enc->frame->width = width;
    enc->frame->height = height;
    enc->frame->color_range = ctx->color_range;
    enc->frame->colorspace = ctx->colorspace;
    enc->frame->color_primaries = ctx->color_primaries;
    enc->frame->color_trc = ctx->color_trc;
    operation = "allocate NV12 frame";
    result = gpu ? av_hwframe_get_buffer(enc->frames,enc->frame,0) : av_frame_get_buffer(enc->frame, 32);
    if (result < 0) goto fail;
    if(!gpu) {
    enc->scale = sws_getContext(source_width, source_height, rgba ? AV_PIX_FMT_RGBA : AV_PIX_FMT_BGRA,
                               width, height, AV_PIX_FMT_NV12,
                               SWS_BILINEAR, NULL, NULL, NULL);
    operation = "create BGRA to NV12 converter";
    result = AVERROR(ENOMEM);
    if (!enc->scale) goto fail;
    operation = "set BT.709 full-range conversion";
    const int *matrix = sws_getCoefficients(SWS_CS_ITU709);
    result = sws_setColorspaceDetails(enc->scale, matrix, 1, matrix, 1, 0, 1 << 16, 1 << 16);
    if (result < 0) goto fail;
    }
    return enc;
fail:
    describe_error(error, error_size, operation, result);
    hypr_nvenc_free(enc);
    return NULL;
}

/* data remains valid until the next encode/free; the caller copies it immediately.
 * Low-latency NVENC must produce exactly the submitted frame, without reordering.
 * A delayed packet is an error rather than silently dropping a damaged region. */
int hypr_nvenc_encode(HyprNvenc *enc, const uint8_t *bgra, int stride, int force_idr,
                      const uint8_t **data, int *size, char *error, size_t error_size) {
    const char *operation = "convert frame";
    int result;
    if(enc->gpu) {
        CUcontext previous;
        if(cuCtxPushCurrent(enc->cuda)!=CUDA_SUCCESS){result=AVERROR_EXTERNAL;goto fail;}
        int sw=enc->source_width,sh=enc->source_height,pitch=sw*4;
        int w=enc->codec->width,h=enc->codec->height,invert=stride<0;
        const uint8_t *base=invert?bgra+(ptrdiff_t)stride*(sh-1):bgra;
        CUDA_MEMCPY2D copy={0};
        copy.srcMemoryType=CU_MEMORYTYPE_HOST;copy.srcHost=base;copy.srcPitch=abs(stride);
        copy.dstMemoryType=CU_MEMORYTYPE_DEVICE;copy.dstDevice=enc->source;copy.dstPitch=pitch;
        copy.WidthInBytes=pitch;copy.Height=sh;
        CUresult cr=cuMemcpy2D(&copy);
        CUdeviceptr yp=(CUdeviceptr)enc->frame->data[0],uv=(CUdeviceptr)enc->frame->data[1];
        int ypitch=enc->frame->linesize[0],uvpitch=enc->frame->linesize[1];
        void *args[]={&enc->source,&sw,&sh,&pitch,&yp,&uv,&ypitch,&uvpitch,&w,&h,&enc->rgba,&invert};
        if(cr==CUDA_SUCCESS) cr=cuLaunchKernel(enc->kernel,(w+31)/32,(h+31)/32,1,16,16,1,0,0,args,NULL);
        if(cr==CUDA_SUCCESS) cr=cuCtxSynchronize();
        cuCtxPopCurrent(&previous);
        if(cr!=CUDA_SUCCESS){result=AVERROR_EXTERNAL;goto fail;}
    } else {
        result=av_frame_make_writable(enc->frame);
        if(result<0)goto fail;
        const uint8_t *planes[4]={bgra,NULL,NULL,NULL};
        int strides[4]={stride,0,0,0};
        result=sws_scale(enc->scale,planes,strides,0,enc->source_height,enc->frame->data,enc->frame->linesize);
        if(result!=enc->codec->height){result=AVERROR(EINVAL);goto fail;}
    }
    int64_t pts = enc->next_pts++;
    enc->frame->pts = pts;
    enc->frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    av_packet_unref(enc->packet);
    operation = "submit NVENC frame";
    result = avcodec_send_frame(enc->codec, enc->frame);
    if (result < 0) goto fail;
    operation = "receive immediate NVENC packet";
    result = avcodec_receive_packet(enc->codec, enc->packet);
    if (result < 0) goto fail;
    if (enc->packet->pts != pts || enc->packet->size <= 0) {
        operation = "NVENC returned a delayed or empty packet";
        result = AVERROR(EINVAL);
        goto fail;
    }
    *data = enc->packet->data;
    *size = enc->packet->size;
    return 0;
fail:
    describe_error(error, error_size, operation, result);
    return result;
}

void hypr_nvenc_bitrate(HyprNvenc *enc, int64_t bitrate) {
    enc->codec->bit_rate=0;
    enc->codec->rc_max_rate=bitrate;
    enc->codec->rc_buffer_size=0;
}
