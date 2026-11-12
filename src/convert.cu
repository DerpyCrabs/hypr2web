// One thread converts a 2x2 output block. Bilinear RGB scaling + BT.709 full-range NV12.
__device__ float3 sample(const unsigned char* src, int sw, int sh, int pitch,
                         int x, int y, int w, int h, int rgba, int invert) {
    float sx=(x+0.5f)*sw/w-0.5f, sy=(y+0.5f)*sh/h-0.5f;
    sx=fminf(fmaxf(sx,0.f),sw-1.f); sy=fminf(fmaxf(sy,0.f),sh-1.f);
    int x0=(int)sx,y0=(int)sy,x1=min(x0+1,sw-1),y1=min(y0+1,sh-1);
    float fx=sx-x0,fy=sy-y0;
    if(invert){y0=sh-1-y0;y1=sh-1-y1;}
    const unsigned char *a=src+y0*pitch+x0*4,*b=src+y0*pitch+x1*4;
    const unsigned char *c=src+y1*pitch+x0*4,*d=src+y1*pitch+x1*4;
    float3 v;
    int r=rgba?0:2,bl=rgba?2:0;
    v.x=(a[r]*(1-fx)+b[r]*fx)*(1-fy)+(c[r]*(1-fx)+d[r]*fx)*fy;
    v.y=(a[1]*(1-fx)+b[1]*fx)*(1-fy)+(c[1]*(1-fx)+d[1]*fx)*fy;
    v.z=(a[bl]*(1-fx)+b[bl]*fx)*(1-fy)+(c[bl]*(1-fx)+d[bl]*fx)*fy;
    return v;
}
__device__ unsigned char byte(float v){return (unsigned char)fminf(255.f,fmaxf(0.f,v+0.5f));}
extern "C" __global__ void rgb_nv12(const unsigned char* src,int sw,int sh,int pitch,
 unsigned char* yp,unsigned char* uv,int ypitch,int uvpitch,int w,int h,int rgba,int invert){
    int x=(blockIdx.x*blockDim.x+threadIdx.x)*2,y=(blockIdx.y*blockDim.y+threadIdx.y)*2;
    if(x>=w||y>=h)return;
    float3 avg=make_float3(0,0,0);
    for(int dy=0;dy<2;dy++)for(int dx=0;dx<2;dx++){
        float3 c=sample(src,sw,sh,pitch,x+dx,y+dy,w,h,rgba,invert);
        yp[(y+dy)*ypitch+x+dx]=byte(0.2126f*c.x+0.7152f*c.y+0.0722f*c.z);
        avg.x+=c.x*.25f;avg.y+=c.y*.25f;avg.z+=c.z*.25f;
    }
    uv[(y/2)*uvpitch+x]=byte(128.f+(-.114572f*avg.x-.385428f*avg.y+.5f*avg.z));
    uv[(y/2)*uvpitch+x+1]=byte(128.f+(.5f*avg.x-.454153f*avg.y-.045847f*avg.z));
}
