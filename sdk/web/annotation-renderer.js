/** Vector annotations in displayed-video coordinates. Never paint text into
 * the decoded camera bitmap: its resolution is unrelated to screen density. */
export const annotationPalette = {accent:'#a78bfa',warning:'#fbbf24',danger:'#f87171',muted:'#cbd5e1',success:'#34d399',series1:'#3b82f6',series2:'#f59e0b',series3:'#10b981',series4:'#6366f1',series5:'#ec4899',series6:'#8b5cf6',series7:'#f97316',series8:'#06b6d4'};
export function annotationMetrics(height) {
  const scale=height/720;
  return {font:Math.max(12,18*scale),stroke:Math.max(1.25,2*scale),
    padding:Math.max(4,6*scale),margin:Math.max(4,8*scale),gap:Math.max(2,4*scale)};
}
export function annotationRect(width,height,videoWidth,videoHeight) {
  const scale=Math.min(width/(videoWidth||width),height/(videoHeight||height));
  const w=(videoWidth||width)*scale,h=(videoHeight||height)*scale;
  return {x:(width-w)/2,y:(height-h)/2,width:w,height:h};
}
function annotationElide(ctx,text,width) {
  if(width<=0)return '';
  if(ctx.measureText(text).width<=width)return text;
  if(ctx.measureText('…').width>width)return '';
  const chars=Array.from(text);let low=0,high=chars.length;
  while(low<high){const mid=Math.ceil((low+high)/2);if(ctx.measureText(chars.slice(0,mid).join('')+'…').width<=width)low=mid;else high=mid-1;}
  return chars.slice(0,low).join('')+'…';
}
export function paintAnnotations(ctx,rect,drawings,{topInset=0,bottomInset=0}={}) {
  if(rect.width<=0||rect.height<=0)return;
  const p=annotationMetrics(rect.height),line=Math.ceil(p.font*1.35),corners=new Map(),textPass=[];
  const left=rect.x+p.margin,right=rect.x+rect.width-p.margin;
  const top=rect.y+Math.max(p.margin,topInset),bottom=rect.y+rect.height-Math.max(p.margin,bottomInset);
  const x=value=>rect.x+value*rect.width,y=value=>rect.y+value*rect.height;
  ctx.save();ctx.beginPath();ctx.rect(rect.x,rect.y,rect.width,rect.height);ctx.clip();
  ctx.font=`600 ${p.font}px system-ui,sans-serif`;ctx.textAlign='left';ctx.textBaseline='alphabetic';ctx.lineWidth=p.stroke;ctx.lineJoin='round';
  function tag(text,atX,atY,color) {
    const shown=annotationElide(ctx,text,Math.max(0,right-left-2*p.padding));if(!shown)return;
    const width=ctx.measureText(shown).width+2*p.padding,height=line+2*p.gap;
    if(bottom-top<height)return;
    const tx=Math.round(Math.max(left,Math.min(right-width,atX))),ty=Math.round(Math.max(top,Math.min(bottom-height,atY)));
    ctx.fillStyle='rgba(8,10,16,0.88)';ctx.fillRect(tx,ty,width,height);ctx.fillStyle=color;
    ctx.fillText(shown,tx+p.padding,ty+p.gap+(line-p.font)/2+p.font*.8);
  }
  for(const drawing of drawings)for(const item of drawing.items) {
    const color=annotationPalette[item.style]||annotationPalette.accent;ctx.strokeStyle=color;ctx.fillStyle=color;
    if(item.type==='text') {
      textPass.push(()=>{
      const anchor=item.anchor||'top_left',offset=corners.get(anchor)||0;
      const count=Math.min(item.lines.length,Math.floor((bottom-top-offset-2*p.gap)/line));if(count<=0)return;
      const lines=item.lines.slice(0,count).map(text=>annotationElide(ctx,text,Math.max(0,right-left-2*p.padding)));
      if(count<item.lines.length)lines[count-1]=annotationElide(ctx,lines[count-1]+'…',Math.max(0,right-left-2*p.padding));
      const width=Math.max(...lines.map(text=>ctx.measureText(text).width))+2*p.padding,height=count*line+2*p.gap;
      const px=Math.round(anchor.endsWith('right')?right-width:left),py=Math.round(anchor.startsWith('bottom')?bottom-offset-height:top+offset);
      ctx.fillStyle='rgba(8,10,16,0.88)';ctx.fillRect(px,py,width,height);ctx.fillStyle=color;
      lines.forEach((text,i)=>ctx.fillText(text,px+p.padding,py+p.gap+i*line+(line-p.font)/2+p.font*.8));
      corners.set(anchor,offset+height+p.gap);
      });continue;
    }
    ctx.beginPath();let corner;
    if(item.type==='box') {const[a,b,w,h]=item.rect;ctx.rect(x(a),y(b),w*rect.width,h*rect.height);corner=[x(a),y(b)];}
    else if(item.points?.length) {item.points.forEach(([a,b],i)=>ctx[i?'lineTo':'moveTo'](x(a),y(b)));if(item.type==='polygon')ctx.closePath();}
    if(item.fill&&item.type!=='polyline'){ctx.globalAlpha=.18;ctx.fill();ctx.globalAlpha=1;}ctx.stroke();
    // Only a box's own label is painted: an area's name is listed under the picture.
    if(item.label&&corner)textPass.push(()=>tag(item.label,corner[0],corner[1]-line-2*p.gap,color));
  }
  for(const drawText of textPass)drawText();
  ctx.restore();
}
export function drawOverlayCanvas(surface,drawings,videoWidth,videoHeight,options={}) {
  const width=surface.clientWidth,height=surface.clientHeight,dpr=options.dpr??globalThis.devicePixelRatio??1;
  const pw=Math.max(1,Math.round(width*dpr)),ph=Math.max(1,Math.round(height*dpr));
  if(surface.width!==pw)surface.width=pw;if(surface.height!==ph)surface.height=ph;
  const ctx=surface.getContext('2d');if(!ctx)return;
  ctx.setTransform(dpr,0,0,dpr,0,0);ctx.clearRect(0,0,width,height);
  if(!width||!height||!videoWidth||!videoHeight)return;
  paintAnnotations(ctx,annotationRect(width,height,videoWidth,videoHeight),drawings,options);
}
