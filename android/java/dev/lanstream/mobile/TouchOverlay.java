package dev.lanstream.mobile;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.Typeface;
import android.util.SparseArray;
import android.view.MotionEvent;
import android.view.View;
import java.util.Arrays;
import java.util.TreeSet;

final class TouchOverlay extends View {
    interface Callbacks {
        void keys(int[] keys);
        void gamepad(int buttons, int x, int y);
        void pointer(int action, int x, int y);
        void edit(Profile.Control control);
        void save();
    }

    private static final int ACCENT = 0xffdbfa72;
    private static final int[] EMPTY = {};
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Path arrow = new Path();
    private final RectF safe = new RectF(), video = new RectF(), picture = new RectF();
    private final SparseArray<Profile.Control> buttons = new SparseArray<>();
    private final Profile profile;
    private final Callbacks callbacks;
    private int stickId = -1, mouseId = -1, editId = -1;
    private float stickX, stickY, mouseX, mouseY, editX, editY, offsetX, offsetY;
    private int[] lastKeys = EMPTY;
    private int lastButtons, lastX, lastY;
    private Profile.Control editingControl;
    private boolean editing, moved, controlsVisible = true;

    TouchOverlay(Context context, Profile profile, Callbacks callbacks) {
        super(context);
        this.profile = profile;
        this.callbacks = callbacks;
        setClickable(true);
        setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_NO);
        paint.setTextAlign(Paint.Align.CENTER);
        paint.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
    }

    void geometry(RectF newSafe, RectF newVideo, RectF newPicture) {
        if (!video.equals(newVideo) && mouseId != -1) {
            sendPointer(2, mouseX, mouseY);
            mouseId = -1;
        }
        safe.set(newSafe);
        video.set(newVideo);
        picture.set(newPicture);
        invalidate();
    }

    void setControlsVisible(boolean value) {
        reset();
        controlsVisible = value;
        invalidate();
    }

    void setEditing(boolean value) {
        reset();
        editing = value;
        invalidate();
    }

    void heartbeat() {
        if (profile.xbox) callbacks.gamepad(lastButtons, lastX, lastY);
        else callbacks.keys(lastKeys);
    }

    void reset() {
        if (mouseId != -1) sendPointer(2, mouseX, mouseY);
        mouseId = stickId = editId = -1;
        editingControl = null;
        stickX = stickY = 0;
        buttons.clear();
        lastKeys = EMPTY;
        lastButtons = lastX = lastY = 0;
        heartbeat();
        invalidate();
    }

    private float dp(float value) { return value * getResources().getDisplayMetrics().density; }

    private float radius(Profile.Control control) {
        return Math.min(dp(control.radius), Math.min(safe.width(), safe.height()) * .22f);
    }

    private float cx(Profile.Control control) {
        float r = radius(control);
        return Profile.clamp(safe.left + control.x * safe.width(), safe.left + r, safe.right - r);
    }

    private float cy(Profile.Control control) {
        float r = radius(control);
        return Profile.clamp(safe.top + control.y * safe.height(), safe.top + r, safe.bottom - r);
    }

    @Override protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        if (safe.isEmpty() || !editing && !controlsVisible) return;
        if (editing) canvas.drawColor(0x900a100d);
        drawStick(canvas);
        for (Profile.Control control : profile.buttons) {
            float x = cx(control), y = cy(control), r = radius(control);
            boolean held = buttons.indexOfValue(control) >= 0 || control == editingControl;
            circle(canvas, x, y, r, held);
            paint.setStyle(Paint.Style.FILL);
            int foreground = Color.WHITE;
            if (profile.xbox && control.key == Profile.PAD_A) foreground = 0xffb9ee79;
            if (profile.xbox && control.key == Profile.PAD_B) foreground = 0xffffa1a0;
            paint.setColor(held ? 0xff1e270c : foreground);
            paint.setTextSize(Math.min(dp(15), r * .64f));
            while (paint.measureText(control.label) > r * 1.63f && paint.getTextSize() > dp(7)) paint.setTextSize(paint.getTextSize() - 1);
            canvas.drawText(control.label, x, y - (paint.ascent() + paint.descent()) / 2, paint);
        }
    }

    private void circle(Canvas canvas, float x, float y, float radius, boolean held) {
        paint.setStyle(Paint.Style.FILL);
        paint.setColor(held ? ACCENT : (editing ? 0xdf1a231e : 0x99191e20));
        canvas.drawCircle(x, y, radius, paint);
        paint.setStyle(Paint.Style.STROKE);
        paint.setStrokeWidth(dp(editing ? 1.5f : 1));
        paint.setColor(held || editing ? ACCENT : 0x99e1eadc);
        canvas.drawCircle(x, y, radius, paint);
        paint.setStyle(Paint.Style.FILL);
    }

    private void drawStick(Canvas canvas) {
        Profile.Control control = profile.stick;
        float x = cx(control), y = cy(control), r = radius(control);
        circle(canvas, x, y, r, control == editingControl);
        paint.setColor(editing ? 0xffd0e3ce : 0xbbe0eedd);
        for (int i = 0; i < 4; i++) {
            canvas.save();
            canvas.rotate(i * 90, x, y);
            arrow.reset();
            arrow.moveTo(x, y - r * .81f);
            arrow.lineTo(x + r * .091f, y - r * .66f);
            arrow.lineTo(x - r * .091f, y - r * .66f);
            arrow.close();
            canvas.drawPath(arrow, paint);
            canvas.restore();
        }
        paint.setColor(stickId == -1 ? 0xbed1dec9 : ACCENT);
        canvas.drawCircle(x + stickX * r * .56f, y + stickY * r * .56f, r * .32f, paint);
    }

    @Override public boolean onTouchEvent(MotionEvent event) {
        if (safe.isEmpty()) return true;
        if (editing) return editTouch(event);
        int action = event.getActionMasked();
        int index = event.getActionIndex();
        int id = event.getPointerId(index);
        float x = event.getX(index), y = event.getY(index);
        switch (action) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                Profile.Control control = controlsVisible ? hit(x, y) : null;
                if (control == profile.stick && stickId == -1) {
                    stickId = id;
                    updateStick(x, y);
                } else if (control != null && control != profile.stick) {
                    buttons.put(id, control);
                } else if (control == null && mouseId == -1 && profile.touchMouse && picture.contains(x, y)) {
                    mouseId = id;
                    sendPointer(1, x, y);
                }
                break;
            case MotionEvent.ACTION_MOVE:
                int stickIndex = event.findPointerIndex(stickId);
                if (stickIndex >= 0) updateStick(event.getX(stickIndex), event.getY(stickIndex));
                int mouseIndex = event.findPointerIndex(mouseId);
                if (mouseIndex >= 0) sendPointer(0, event.getX(mouseIndex), event.getY(mouseIndex));
                break;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_POINTER_UP:
                if (id == stickId) { stickId = -1; stickX = stickY = 0; }
                if (id == mouseId) { sendPointer(2, x, y); mouseId = -1; }
                buttons.remove(id);
                performClick();
                break;
            case MotionEvent.ACTION_CANCEL:
                reset();
                return true;
            default:
                return true;
        }
        if (profile.xbox) updateGamepad(); else updateKeys();
        invalidate();
        return true;
    }

    @Override public boolean performClick() { return super.performClick(); }

    private Profile.Control hit(float x, float y) {
        for (int index = profile.buttons.size() - 1; index >= 0; index--) {
            Profile.Control control = profile.buttons.get(index);
            if (hit(control, x, y)) return control;
        }
        return hit(profile.stick, x, y) ? profile.stick : null;
    }

    private boolean hit(Profile.Control control, float x, float y) {
        float dx = x - cx(control), dy = y - cy(control), r = radius(control) + dp(4);
        return dx * dx + dy * dy < r * r;
    }

    private void updateStick(float x, float y) {
        float r = radius(profile.stick), dx = (x - cx(profile.stick)) / r, dy = (y - cy(profile.stick)) / r;
        float norm = Math.max(1, (float)Math.hypot(dx, dy));
        stickX = dx / norm;
        stickY = dy / norm;
    }

    private void updateKeys() {
        TreeSet<Integer> keys = new TreeSet<>();
        for (int i = 0; i < buttons.size(); i++) keys.add(buttons.valueAt(i).key);
        if (Math.hypot(stickX, stickY) >= .24) {
            int octant = (int)Math.round(Math.atan2(stickY, stickX) / (Math.PI / 4));
            if (octant <= -1 && octant >= -3) keys.add(profile.directions[0]);
            if (Math.abs(octant) >= 3) keys.add(profile.directions[1]);
            if (octant >= 1 && octant <= 3) keys.add(profile.directions[2]);
            if (Math.abs(octant) <= 1) keys.add(profile.directions[3]);
        }
        int[] current = new int[keys.size()];
        int i = 0;
        for (int key : keys) current[i++] = key;
        if (!Arrays.equals(lastKeys, current)) { lastKeys = current; heartbeat(); }
    }

    private void updateGamepad() {
        int pressed = 0;
        for (int i = 0; i < buttons.size(); i++) pressed |= buttons.valueAt(i).key;
        float magnitude = (float)Math.hypot(stickX, stickY);
        float gain = magnitude <= .12f ? 0 : Math.min(1, (magnitude - .12f) / .88f) / magnitude;
        int x = Math.round(stickX * gain * 32767), y = Math.round(-stickY * gain * 32767);
        if (lastButtons == pressed && lastX == x && lastY == y) return;
        lastButtons = pressed; lastX = x; lastY = y;
        heartbeat();
    }

    private void sendPointer(int action, float x, float y) {
        if (video.isEmpty()) return;
        mouseX = x; mouseY = y;
        callbacks.pointer(action, (int)(65535 * Profile.clamp((x - video.left) / video.width(), 0, 1)),
            (int)(65535 * Profile.clamp((y - video.top) / video.height(), 0, 1)));
    }

    private boolean editTouch(MotionEvent event) {
        int action = event.getActionMasked();
        int index = event.getActionIndex();
        float x = event.getX(index), y = event.getY(index);
        if (action == MotionEvent.ACTION_DOWN) {
            editingControl = hit(x, y);
            if (editingControl != null) {
                editId = event.getPointerId(index);
                editX = x; editY = y;
                offsetX = x - cx(editingControl); offsetY = y - cy(editingControl);
                moved = false;
            }
        } else if (action == MotionEvent.ACTION_MOVE && editingControl != null) {
            int owner = event.findPointerIndex(editId);
            if (owner >= 0) {
                x = event.getX(owner); y = event.getY(owner);
                moved |= Math.hypot(x - editX, y - editY) > dp(6);
                if (moved) {
                    editingControl.x = Profile.clamp((x - offsetX - safe.left) / safe.width(), 0, 1);
                    editingControl.y = Profile.clamp((y - offsetY - safe.top) / safe.height(), 0, 1);
                }
            }
        } else if ((action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_POINTER_UP) && event.getPointerId(index) == editId) {
            Profile.Control selected = editingControl;
            editingControl = null;
            editId = -1;
            if (selected != null) {
                if (moved) callbacks.save(); else callbacks.edit(selected);
            }
            performClick();
        } else if (action == MotionEvent.ACTION_CANCEL) {
            if (moved) callbacks.save();
            editingControl = null;
            editId = -1;
        }
        invalidate();
        return true;
    }
}
