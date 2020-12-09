//
//  GetUTXOsViewController.swift
//  ZcashLightClientSample
//
//  Created by Francisco Gindre on 12/9/20.
//  Copyright © 2020 Electric Coin Company. All rights reserved.
//

import UIKit
import ZcashLightClientKit
import KRProgressHUD
class GetUTXOsViewController: UIViewController {
    @IBOutlet weak var tAddressField: UITextField!
    @IBOutlet weak var getButton: UIButton!
    @IBOutlet weak var validAddressLabel: UILabel!
    @IBOutlet weak var messageLabel: UILabel!
    
    var service: LightWalletGRPCService = LightWalletGRPCService(endpoint: DemoAppConfig.endpoint)
    
    override func viewDidLoad() {
        super.viewDidLoad()
        
        updateUI()
        
    }
    
    func updateUI() {
        let valid = Initializer.shared.isValidTransparentAddress(tAddressField.text ?? "")
        
        self.validAddressLabel.text = valid ? "Valid TransparentAddress" : "Invalid Transparent address"
        self.validAddressLabel.textColor = valid ? UIColor.systemGreen : UIColor.systemRed
        
        self.getButton.isEnabled = valid
    }
    
    @IBAction func getButtonTapped(_ sender: Any) {
        guard Initializer.shared.isValidTransparentAddress(tAddressField.text ?? ""),
              let tAddr = tAddressField.text else {
            return
        }
        KRProgressHUD.showMessage("fetching")
        service.fetchUTXOs(for: tAddr) { [weak self] (result) in
            DispatchQueue.main.async { [weak self] in
                KRProgressHUD.dismiss()
                switch result {
                case .success(let utxos):
                    self?.messageLabel.text  = "found \(utxos.count) UTXOs for address \(tAddr)"
                    
                case .failure(let error):
                    self?.messageLabel.text = "Error \(error)"
                }
            }
        }
    }
    
    @IBAction func viewTapped(_ recognizer: UITapGestureRecognizer)  {
        self.tAddressField.resignFirstResponder()
    }
}

extension GetUTXOsViewController: UITextFieldDelegate {
    func textField(_ textField: UITextField, shouldChangeCharactersIn range: NSRange, replacementString string: String) -> Bool {
        updateUI()
        return true
    }
    
    func textFieldDidEndEditing(_ textField: UITextField)  {
        updateUI()
    }
    
}