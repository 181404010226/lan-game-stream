package dev.lanstream.mobile;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;
import java.util.ArrayList;
import java.util.LinkedHashMap;

final class Profile {
    static final int MAX_BUTTONS = 14;
    static final int PAD_A = 0x1000, PAD_B = 0x2000, PAD_MENU = 0x0010;
    static final LinkedHashMap<Integer, String> KEYS = new LinkedHashMap<>();
    private static final LinkedHashMap<Integer, String> PAD_KEYS = new LinkedHashMap<>();
    static {
        KEYS.put(13, "Enter · 回车"); KEYS.put(27, "Esc · 返回"); KEYS.put(32, "Space · 空格");
        KEYS.put(37, "←"); KEYS.put(38, "↑"); KEYS.put(39, "→"); KEYS.put(40, "↓");
        for (int c = 'A'; c <= 'Z'; c++) KEYS.put(c, Character.toString((char)c));
        for (int c = '0'; c <= '9'; c++) KEYS.put(c, Character.toString((char)c));
        KEYS.put(1, "鼠标左键"); KEYS.put(2, "鼠标右键"); KEYS.put(4, "鼠标中键");
        KEYS.put(8, "Backspace"); KEYS.put(9, "Tab"); KEYS.put(16, "Shift"); KEYS.put(17, "Ctrl");
        KEYS.put(33, "Page Up"); KEYS.put(34, "Page Down"); KEYS.put(35, "End"); KEYS.put(36, "Home");
        KEYS.put(45, "Insert"); KEYS.put(46, "Delete");
        for (int i = 1; i <= 12; i++) KEYS.put(111 + i, "F" + i);
        String[] symbols = {";", "=", ",", "-", ".", "/", "`", "[", "\\", "]", "'"};
        int[] codes = {186,187,188,189,190,191,192,219,220,221,222};
        for (int i = 0; i < codes.length; i++) KEYS.put(codes[i], symbols[i]);
        PAD_KEYS.put(PAD_A, "A · 确认"); PAD_KEYS.put(PAD_B, "B · 返回"); PAD_KEYS.put(PAD_MENU, "菜单键");
        PAD_KEYS.put(0x4000, "X"); PAD_KEYS.put(0x8000, "Y"); PAD_KEYS.put(0x0020, "视图键");
        PAD_KEYS.put(0x0100, "LB"); PAD_KEYS.put(0x0200, "RB");
        PAD_KEYS.put(0x0001, "方向键 ↑"); PAD_KEYS.put(0x0002, "方向键 ↓");
        PAD_KEYS.put(0x0004, "方向键 ←"); PAD_KEYS.put(0x0008, "方向键 →");
        PAD_KEYS.put(0x0040, "左摇杆按下"); PAD_KEYS.put(0x0080, "右摇杆按下");
    }

    enum Mode {
        AUTO("自动铺屏"), FIT("完整画面"), RAILS("两侧控件"), FILL("铺满裁切"), PHONE("手机画幅");
        final String label;
        Mode(String label) { this.label = label; }
        boolean requestsPhone() { return this == AUTO || this == PHONE; }
    }

    static final class Control {
        String label;
        float x, y, radius;
        int key;

        Control(String label, float x, float y, float radius, int key) {
            this.label = label;
            this.x = x;
            this.y = y;
            this.radius = radius;
            this.key = key;
        }

        JSONObject json() throws JSONException {
            return new JSONObject().put("label", label).put("x", x).put("y", y).put("radius", radius).put("key", key);
        }

        static Control read(JSONObject data, String fallback, int key, LinkedHashMap<Integer, String> bindings) {
            int mapped = data.optInt("key", key);
            String label = data.optString("label", fallback);
            return new Control(label.substring(0, Math.min(label.length(), 8)),
                clamp((float)data.optDouble("x", .5), 0, 1), clamp((float)data.optDouble("y", .5), 0, 1),
                clamp((float)data.optDouble("radius", 28), 18, 85), bindings.containsKey(mapped) ? mapped : key);
        }
    }

    final ArrayList<Control> buttons = new ArrayList<>();
    final boolean xbox;
    Control stick = new Control("摇杆", .073f, .73f, 52, 0);
    int[] directions = {'W', 'A', 'S', 'D'};
    Mode mode = Mode.AUTO;
    boolean touchMouse;

    private Profile(boolean xbox) {
        this.xbox = xbox;
        touchMouse = !xbox;
        if (xbox) {
            buttons.add(new Control("A", .879f, .86f, 26, PAD_A));
            buttons.add(new Control("B", .938f, .72f, 25, PAD_B));
            buttons.add(new Control("≡", .93f, .46f, 19, PAD_MENU));
        } else {
            buttons.add(new Control("确认", .933f, .71f, 25, 13));
            buttons.add(new Control("返回", .933f, .88f, 25, 27));
            buttons.add(new Control("暂停", .93f, .48f, 23, 27));
        }
    }

    LinkedHashMap<Integer, String> bindings() { return xbox ? PAD_KEYS : KEYS; }

    static Profile load(String saved, boolean xbox) {
        Profile profile = new Profile(xbox);
        if (saved == null) return profile;
        try {
            JSONObject data = new JSONObject(saved);
            if (data.optInt("screen_layout_version") >= 2) profile.mode = Mode.valueOf(data.optString("mode", Mode.AUTO.name()));
            profile.stick = Control.read(data.getJSONObject("stick"), "摇杆", 0, profile.bindings());
            if (data.optBoolean("xbox") != xbox) return profile;
            profile.touchMouse = data.optBoolean("mouse", !xbox);
            JSONArray directions = data.getJSONArray("directions");
            for (int i = 0; i < 4; i++) {
                int key = directions.optInt(i, profile.directions[i]);
                if (KEYS.containsKey(key)) profile.directions[i] = key;
            }
            JSONArray buttons = data.getJSONArray("buttons");
            profile.buttons.clear();
            for (int i = 0; i < Math.min(buttons.length(), MAX_BUTTONS); i++) {
                profile.buttons.add(Control.read(buttons.getJSONObject(i), "按键", xbox ? PAD_A : 32, profile.bindings()));
            }
            return profile;
        } catch (IllegalArgumentException | JSONException malformed) {
            return new Profile(xbox);
        }
    }

    String save() {
        try {
            JSONArray keys = new JSONArray();
            for (int key : directions) keys.put(key);
            JSONArray controls = new JSONArray();
            for (Control control : buttons) controls.put(control.json());
            return new JSONObject().put("screen_layout_version", 2).put("mode", mode.name()).put("xbox", xbox).put("mouse", touchMouse).put("stick", stick.json())
                .put("directions", keys).put("buttons", controls).toString();
        } catch (JSONException invalid) {
            throw new IllegalStateException(invalid);
        }
    }

    static float clamp(float value, float min, float max) {
        return Math.max(min, Math.min(max, value));
    }
}
