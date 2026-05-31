-- Sakurasato M11 — Update Note 受信用に edited_at を追加。
--
-- リモート Note の編集 (`Update`/`Note`) を受け取ったときに、TUI 側で
-- 「いつ編集されたか」を表示できるよう、編集時刻を別カラムで保持する。
-- `updated_at` は row の任意の更新で動くので、編集時刻として再利用すると
-- (例: アバター変更などの metadata 更新でも触れた場合に) 意味が壊れる。
--
-- 初回受信時は NULL。Update 受信時に object.updated (または受信時刻) を
-- 入れる。ローカル投稿の編集機能は M? 以降だが、列としては共通化しておく。
ALTER TABLE note ADD COLUMN edited_at TIMESTAMPTZ;
