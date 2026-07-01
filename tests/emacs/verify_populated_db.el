;;; verify_populated_db.el --- verify org-roam can read a Rust-populated DB  -*- lexical-binding: t -*-

;; Inputs (set via --eval before --load):
;;   org-roam-directory  - directory containing the .org files
;;   org-roam-db-location - path to the Rust-created org-roam.db
;;   target-node-id      - ID we expect org-roam to find

(require 'package)
(package-initialize)
(require 'json)
(require 'org-roam)

;; org-roam-db-sync reads the existing DB and reconciles it against disk.
(org-roam-db-sync)

(let ((node (org-roam-node-from-id target-node-id)))
  (if (null node)
      (progn
        (princ (json-encode '(("found" . :json-false))))
        (terpri)
        (kill-emacs 1))
    (princ (json-encode (list (cons "found" t)
                              (cons "id" (org-roam-node-id node))
                              (cons "title" (org-roam-node-title node))
                              (cons "file" (org-roam-node-file node)))))
    (terpri)))
