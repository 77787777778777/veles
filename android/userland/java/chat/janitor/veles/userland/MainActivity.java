package chat.janitor.veles.userland;

import android.app.Activity;
import android.graphics.Typeface;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.TypedValue;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

/**
 * The userland's state, Start and Stop, and its log (the rootfs
 * extraction's time and size, the broker's connections). Opening it
 * starts the service; so does the gateway binding it (UserlandService).
 */
public final class MainActivity extends Activity {
    private static final int LOG_TAIL = 32 << 10;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private TextView status;
    private TextView log;

    private final Runnable tick = new Runnable() {
        @Override
        public void run() {
            status.setText(UserlandService.state);
            log.setText(UserlandService.tail(UserlandService.log(MainActivity.this), LOG_TAIL));
            ui.postDelayed(this, 1000);
        }
    };

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        int pad = Math.round(16 * getResources().getDisplayMetrics().density);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(pad, pad, pad, pad);
        status = new TextView(this);
        status.setTextSize(TypedValue.COMPLEX_UNIT_SP, 20);
        root.addView(status);
        LinearLayout buttons = new LinearLayout(this);
        Button start = new Button(this);
        start.setText(R.string.action_start);
        start.setOnClickListener(v -> UserlandService.start(this));
        Button stop = new Button(this);
        stop.setText(R.string.action_stop);
        stop.setOnClickListener(v -> UserlandService.stop(this));
        buttons.addView(start);
        buttons.addView(stop);
        root.addView(buttons);
        TextView title = new TextView(this);
        title.setText(R.string.log_title);
        title.setTypeface(Typeface.DEFAULT_BOLD);
        root.addView(title);
        log = new TextView(this);
        log.setTypeface(Typeface.MONOSPACE);
        log.setTextSize(TypedValue.COMPLEX_UNIT_SP, 11);
        log.setTextIsSelectable(true);
        ScrollView scroll = new ScrollView(this);
        scroll.addView(log);
        root.addView(scroll, new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1));
        setContentView(root);
    }

    @Override
    protected void onResume() {
        super.onResume();
        UserlandService.start(this);
        ui.post(tick);
    }

    @Override
    protected void onPause() {
        ui.removeCallbacks(tick);
        super.onPause();
    }
}
