"""NV12 texture rendering; CPU bytes remain owned until texture upload completes."""

from __future__ import annotations

import logging
import os
import struct
from collections.abc import Callable
from functools import lru_cache

from PySide6.QtCore import QRect, Qt, QTimer, Signal
from PySide6.QtGui import QOffscreenSurface, QOpenGLContext, QPainter, QSurfaceFormat
from PySide6.QtOpenGL import QOpenGLBuffer, QOpenGLShader, QOpenGLShaderProgram, QOpenGLTexture
from PySide6.QtOpenGLWidgets import QOpenGLWidget
from PySide6.QtWidgets import QApplication, QWidget

logger = logging.getLogger(__name__)
# OpenGL 2 / GLES 2 constants. Qt resolves the platform's GL functions.
_TEXTURE_2D, _TEXTURE0, _UNSIGNED_BYTE = 0x0DE1, 0x84C0, 0x1401
_LUMINANCE, _LUMINANCE_ALPHA, _FLOAT = 0x1909, 0x190A, 0x1406
_VERTEX = """
attribute vec4 vertex;
uniform vec4 targetRect;
varying vec2 uv;
void main() {
    gl_Position = vec4(targetRect.xy + vertex.xy * targetRect.zw, 0.0, 1.0);
    uv = vertex.zw;
}
"""
_FRAGMENT = """
#ifdef GL_ES
precision highp float;
#endif
varying vec2 uv;
uniform sampler2D yPlane;
uniform sampler2D uvPlane;
uniform vec4 levels;
uniform vec4 coefficients;
uniform vec4 chromaCoords;
void main() {
    float y = (texture2D(yPlane, uv).r - levels.x) * levels.y;
    vec2 c = (texture2D(uvPlane, uv * chromaCoords.xy + chromaCoords.zw).ra
              - vec2(levels.z)) * levels.w;
    gl_FragColor = vec4(y + coefficients.x * c.y,
                       y + coefficients.y * c.x + coefficients.z * c.y,
                       y + coefficients.w * c.x, 1.0);
}
"""


def video_surface_format() -> QSurfaceFormat:
    fmt = QSurfaceFormat()
    fmt.setVersion(2, 0)
    fmt.setProfile(QSurfaceFormat.OpenGLContextProfile.NoProfile)
    fmt.setDepthBufferSize(0)
    fmt.setStencilBufferSize(0)
    return fmt


@lru_cache(maxsize=1)
def gpu_video_available() -> bool:
    """Probe only on the GUI thread; software/offscreen contexts keep RGB."""
    app = QApplication.instance()
    if not isinstance(app, QApplication) or app.platformName() in ("offscreen", "minimal"):
        return False
    if os.getenv("NW_GPU_VIDEO") == "0":
        return False
    context = QOpenGLContext()
    context.setFormat(video_surface_format())
    if not context.create():
        return False
    surface = QOffscreenSurface()
    surface.setFormat(context.format())
    surface.create()
    if not context.makeCurrent(surface):
        return False
    try:
        renderer = context.functions().glGetString(0x1F01).lower()
        supported = not any(
            name in renderer
            for name in ("llvmpipe", "softpipe", "software", "swiftshader", "swrast", "gdi generic")
        )
        logger.info(
            "GPU preview renderer: %s (%s)", renderer, "enabled" if supported else "RGB fallback"
        )
        return supported
    except Exception:
        logger.exception("GPU preview probe failed; using RGB")
        return False
    finally:
        context.doneCurrent()
        surface.destroy()


def reserve_gl_window(widget: QWidget) -> QWidget:
    """Give ``widget``'s future window a hidden, never-initialized GL child.

    The first QOpenGLWidget in a visible window makes Qt switch that window
    to GL composition by destroying and recreating it: on screen the window
    closes and opens again (at startup, when the first video frame arrives).
    A GL child present before the window is first shown makes Qt create it
    GL-ready, so later video surfaces join without a recreation.
    """
    placeholder = QOpenGLWidget(widget)
    placeholder.setObjectName("glWindowPlaceholder")
    placeholder.setFormat(video_surface_format())
    placeholder.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
    placeholder.setFixedSize(0, 0)
    placeholder.hide()
    return placeholder


def colour_parameters(descriptor: int) -> tuple[tuple[float, ...], tuple[float, ...]]:
    if descriptor & ~0x73 or descriptor & 3 not in (1, 2, 3):
        raise ValueError("unsupported NV12 colour descriptor")
    kr, kb = {1: (0.299, 0.114), 2: (0.2126, 0.0722), 3: (0.2627, 0.0593)}[descriptor & 3]
    kg = 1.0 - kr - kb
    coefficients = (2 * (1 - kr), -2 * kb * (1 - kb) / kg, -2 * kr * (1 - kr) / kg, 2 * (1 - kb))
    levels = (
        (0.0, 1.0, 128 / 255, 1.0)
        if descriptor & 16
        else (16 / 255, 255 / 219, 128 / 255, 255 / 224)
    )
    return levels, coefficients


class GpuVideoSurface(QOpenGLWidget):
    failed = Signal()

    def __init__(
        self,
        parent: QWidget,
        target_rect: Callable[[int, int], QRect],
        draw_overlays: Callable[[QPainter], None],
    ) -> None:
        super().__init__(parent)
        self.setFormat(video_surface_format())
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
        self.setAutoFillBackground(False)
        self._target_rect = target_rect
        self._draw_overlays = draw_overlays
        self._program: QOpenGLShaderProgram | None = None
        self._buffer: QOpenGLBuffer | None = None
        self._textures: list[QOpenGLTexture] = []
        self._frame: tuple[bytes, int, int, int] | None = None
        self._texture_size = (0, 0)
        self._dirty = False
        self._failed = False
        self.upload_count = 0

    def set_frame(self, data: bytes, width: int, height: int, descriptor: int) -> None:
        colour_parameters(descriptor)
        expected = width * height + ((width + 1) // 2) * ((height + 1) // 2) * 2
        if width <= 0 or height <= 0 or len(data) != expected:
            raise ValueError("invalid NV12 frame size")
        self._frame = (data, width, height, descriptor)
        self._dirty = True
        self.update()

    def _fail(self, error: object) -> None:
        if self._failed:
            return
        self._failed = True
        logger.warning("GPU preview failed; requesting RGB fallback: %s", error)
        QTimer.singleShot(0, self.failed.emit)

    def initializeGL(self) -> None:  # noqa: N802
        try:
            self.context().aboutToBeDestroyed.connect(self.cleanup)
            program = QOpenGLShaderProgram(self)
            if not program.addShaderFromSourceCode(QOpenGLShader.ShaderTypeBit.Vertex, _VERTEX):
                raise RuntimeError(program.log())
            if not program.addShaderFromSourceCode(QOpenGLShader.ShaderTypeBit.Fragment, _FRAGMENT):
                raise RuntimeError(program.log())
            program.bindAttributeLocation("vertex", 0)
            if not program.link():
                raise RuntimeError(program.log())
            self._program = program
            self._buffer = QOpenGLBuffer(QOpenGLBuffer.Type.VertexBuffer)
            if not self._buffer.create() or not self._buffer.bind():
                raise RuntimeError("vertex buffer creation failed")
            vertices = struct.pack("16f", -1, -1, 0, 1, 1, -1, 1, 1, -1, 1, 0, 0, 1, 1, 1, 0)
            self._buffer.allocate(vertices, len(vertices))
            self._buffer.release()
            for _ in range(2):
                texture = QOpenGLTexture(QOpenGLTexture.Target.Target2D)
                if not texture.create():
                    raise RuntimeError("texture creation failed")
                self._textures.append(texture)
            self._texture_size = (0, 0)
            self._dirty = self._frame is not None
        except Exception as error:
            self._fail(error)

    def paintGL(self) -> None:  # noqa: N802
        if self._failed or self._program is None or self._buffer is None or self._frame is None:
            return
        try:
            gl = self.context().functions()
            gl.glDisable(0x0BE2)  # blending may have been enabled by QPainter
            gl.glDisable(0x0B71)  # depth test
            gl.glDisable(0x0C11)  # scissor test
            gl.glColorMask(True, True, True, True)
            gl.glClearColor(18 / 255, 18 / 255, 28 / 255, 1.0)
            gl.glClear(0x00004000)
            dpr = self.devicePixelRatioF()
            gl.glViewport(0, 0, round(self.width() * dpr), round(self.height() * dpr))
            data, width, height, descriptor = self._frame
            cw, ch = (width + 1) // 2, (height + 1) // 2
            planes = memoryview(data)  # slicing a view does not copy pixels
            gl.glPixelStorei(0x0CF5, 1)  # tightly packed planes, including odd widths
            for plane, (pw, ph, fmt, offset) in enumerate(
                [(width, height, _LUMINANCE, 0), (cw, ch, _LUMINANCE_ALPHA, width * height)]
            ):
                gl.glActiveTexture(_TEXTURE0 + plane)
                gl.glBindTexture(_TEXTURE_2D, self._textures[plane].textureId())
                if self._texture_size != (width, height):
                    gl.glTexParameteri(_TEXTURE_2D, 0x2801, 0x2601)  # min/mag linear
                    gl.glTexParameteri(_TEXTURE_2D, 0x2800, 0x2601)
                    gl.glTexParameteri(_TEXTURE_2D, 0x2802, 0x812F)  # clamp to edge
                    gl.glTexParameteri(_TEXTURE_2D, 0x2803, 0x812F)
                    # PySide accepts the buffer protocol; its stub incorrectly says int.
                    gl.glTexImage2D(
                        _TEXTURE_2D,
                        0,
                        fmt,
                        pw,
                        ph,
                        0,
                        fmt,
                        _UNSIGNED_BYTE,
                        planes[offset:],  # type: ignore[arg-type]
                    )
                elif self._dirty:
                    gl.glTexSubImage2D(
                        _TEXTURE_2D,
                        0,
                        0,
                        0,
                        pw,
                        ph,
                        fmt,
                        _UNSIGNED_BYTE,
                        planes[offset:],  # type: ignore[arg-type]
                    )
            if self._dirty:
                self.upload_count += 1
            self._dirty = False
            self._texture_size = width, height
            program = self._program
            program.bind()
            gl.glUniform1i(program.uniformLocation("yPlane"), 0)
            gl.glUniform1i(program.uniformLocation("uvPlane"), 1)
            levels, coefficients = colour_parameters(descriptor)
            program.setUniformValue(program.uniformLocation("levels"), *levels)
            program.setUniformValue(program.uniformLocation("coefficients"), *coefficients)
            program.setUniformValue(
                program.uniformLocation("chromaCoords"),
                width / (2 * cw),
                height / (2 * ch),
                0.25 / cw if descriptor & 32 else 0.0,
                0.25 / ch if descriptor & 64 else 0.0,
            )
            rect = self._target_rect(width, height)
            program.setUniformValue(
                program.uniformLocation("targetRect"),
                (2 * rect.x() + rect.width()) / self.width() - 1,
                1 - (2 * rect.y() + rect.height()) / self.height(),
                rect.width() / self.width(),
                rect.height() / self.height(),
            )
            self._buffer.bind()
            program.enableAttributeArray(0)
            program.setAttributeBuffer(0, _FLOAT, 0, 4, 16)
            gl.glDrawArrays(0x0005, 0, 4)  # triangle strip
            program.disableAttributeArray(0)
            self._buffer.release()
            program.release()
            gl.glActiveTexture(_TEXTURE0)
            painter = QPainter(self)
            self._draw_overlays(painter)
            painter.end()
        except Exception as error:
            self._fail(error)

    def cleanup(self) -> None:
        if self.context() is None:
            return
        self.makeCurrent()
        for texture in self._textures:
            texture.destroy()
        self._textures.clear()
        if self._buffer is not None:
            self._buffer.destroy()
            self._buffer = None
        if self._program is not None:
            self._program.removeAllShaders()
            self._program.deleteLater()
            self._program = None
        self._texture_size = (0, 0)
        self.doneCurrent()
