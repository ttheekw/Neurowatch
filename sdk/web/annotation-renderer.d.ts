export interface AnnotationRect {x:number;y:number;width:number;height:number}
export interface AnnotationOptions {dpr?:number;topInset?:number;bottomInset?:number}
export interface AnnotationLayer {items:({type:'box';rect:[number,number,number,number];label?:string;style?:string;fill?:boolean}|{type:'polygon'|'polyline';points:[number,number][];label?:string;style?:string;fill?:boolean}|{type:'text';anchor?:string;lines:string[];style?:string})[]}
export const annotationPalette:Record<string,string>;
export function annotationMetrics(height:number):{font:number;stroke:number;padding:number;margin:number;gap:number};
export function annotationRect(width:number,height:number,videoWidth:number,videoHeight:number):AnnotationRect;
export function paintAnnotations(ctx:CanvasRenderingContext2D,rect:AnnotationRect,drawings:AnnotationLayer[],options?:AnnotationOptions):void;
export function drawOverlayCanvas(surface:HTMLCanvasElement,drawings:AnnotationLayer[],videoWidth:number,videoHeight:number,options?:AnnotationOptions):void;
